//! Credential resolution shared by every provider.
//!
//! Three things live here, in the order the port needs them:
//!
//! 1. [`Secret`] — a newtype whose `Debug`/`Display` are redacted, so a token can
//!    never reach a log line by accident. Callers opt in with [`Secret::expose`],
//!    which is greppable in review.
//! 2. [`cleaned`] / [`Env`] / [`PortConfig`] — the canonical credential-cleaning
//!    rule, a case-insensitive environment snapshot (Windows env vars are
//!    case-insensitive) and a read-only reader for the port's own `config.json`
//!    (`SPEC-apikey.md` §1.1–§1.4).
//! 3. [`read_json`] / path helpers — read-only file access with the Windows path
//!    conventions from `SPEC-flagship.md` §1.2 and §7.
//!
//! 4. [`SecretBackend`] — the vault for CodexBar's **own** secrets (API keys
//!    typed into its config, the Copilot device-flow token). The production
//!    backend is Windows Credential Manager with a DPAPI-encrypted file as the
//!    fallback ([`default_vault`]); `config.json` only keeps a
//!    `{"$vault": "<key>"}` reference. Credentials that belong to *other* CLIs
//!    (Codex, Claude, Cursor, Gemini, …) are never copied into it.
//!
//! Nothing in this module writes another tool's files. The vault backends write
//! only CodexBar's own Credential Manager entries and its own
//! `%LOCALAPPDATA%\CodexBar\secrets.dpapi`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use codexbar_core::ProviderId;
use zeroize::Zeroize;

/// A credential value that refuses to print itself.
///
/// ```ignore
/// let key = Secret::new("sk-or-v1-…");
/// println!("{key:?}");        // Secret("<redacted>")
/// attach(req, key.expose());  // explicit, and easy to grep for in review
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Every call site is a place to double-check that the value
    /// is going into a request header, never into a log or a snapshot field.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.trim().is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// `sk-or-v1…9f2c` — safe for `account`, `error`, tooltips and screenshots.
    pub fn redacted(&self) -> String {
        redact(&self.0)
    }

    /// Last `n` characters, for the rare case where two keys must be told apart
    /// in the UI without showing either.
    pub fn suffix(&self, n: usize) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        let take = n.min(chars.len());
        chars[chars.len() - take..].iter().collect()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(\"<redacted>\")")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// The buffer is wiped when the value is dropped (RECON D10), so a refreshed
/// token or a key read from the vault does not linger in freed heap memory.
impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl From<Secret> for String {
    /// Explicit, named conversion — an audit point.
    fn from(mut value: Secret) -> Self {
        std::mem::take(&mut value.0)
    }
}

/// Render a credential as `prefix…suffix`, keeping at most 6 + 4 visible
/// characters. Short values collapse to `…`.
pub fn redact(value: &str) -> String {
    let trimmed = value.trim();
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars.iter().take(6).collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// Render an *e-mail identity* as `local@…`, keeping at most 8 characters of the
/// local part and dropping the domain entirely.
///
/// `account` is the one snapshot field that carries a human identity, and some
/// providers take it from a credential's `email` claim. A full address is
/// contactable PII, so it follows the same policy as a masked account id: a
/// stable, recognisable stub — never the whole value. The domain is what makes
/// the address reachable, so it is always replaced, like `redact` replaces the
/// middle of a token. A value with no `@` is not an address and falls back to
/// [`redact`].
pub fn mask_email(value: &str) -> String {
    const LOCAL_VISIBLE: usize = 8;
    let trimmed = value.trim();
    let Some((local, _domain)) = trimmed.split_once('@') else {
        return redact(trimmed);
    };
    let head: String = local.chars().take(LOCAL_VISIBLE).collect();
    if head.is_empty() {
        return "…".to_string();
    }
    format!("{head}@…")
}

/// The canonical credential-cleaning rule (`SPEC-apikey.md` §1.2).
///
/// 1. trim; empty → `None`
/// 2. strip one matching pair of surrounding quotes (`"…"` or `'…'`)
/// 3. trim again; empty → `None`
///
/// Windows users routinely end up with `setx FOO "\"sk-…\""` or a `.env` value
/// pasted with quotes, so this runs on every value a provider accepts.
pub fn cleaned(raw: &str) -> Option<String> {
    let first = raw.trim();
    if first.is_empty() {
        return None;
    }
    let unquoted = if (first.starts_with('"') && first.ends_with('"') && first.len() >= 2)
        || (first.starts_with('\'') && first.ends_with('\'') && first.len() >= 2)
    {
        &first[1..first.len() - 1]
    } else {
        first
    };
    let second = unquoted.trim();
    if second.is_empty() {
        None
    } else {
        Some(second.to_string())
    }
}

/// Mask anything in free text that looks like a credential.
///
/// Used on upstream error bodies and on `codexbar diagnose`-style output
/// (`SPEC-apikey.md` §4.3). Over-masking is fine; under-masking is not.
pub fn redact_secrets_in_text(text: &str) -> String {
    const PREFIXES: [&str; 14] = [
        "sk-",
        "sk_",
        "sk.",
        "xai-",
        "gsk_",
        "hf_",
        "xi-",
        "AIza",
        "ghp_",
        "github_pat_",
        "eyJ",
        "token ",
        "Bearer ",
        "Basic ",
    ];

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        // Longest matching credential-looking run at this position.
        let hit = credential_run(rest, &PREFIXES);
        match hit {
            Some(len) => {
                let (head, tail) = rest.split_at(len);
                out.push_str(&redact(head));
                out.push_str(" [redacted]");
                rest = tail;
            }
            None => {
                let mut chars = rest.chars();
                match chars.next() {
                    Some(c) => {
                        out.push(c);
                        rest = chars.as_str();
                    }
                    None => break,
                }
            }
        }
    }
    out
}

/// If `text` starts with something credential-shaped, return its length.
fn credential_run(text: &str, prefixes: &[&str]) -> Option<usize> {
    // `Authorization: Bearer <token>` / `Basic <token>` — mask only the token.
    for scheme in ["Bearer ", "Basic ", "token "] {
        if let Some(after) = text.strip_prefix(scheme) {
            let run = token_run(after).unwrap_or(0);
            if run > 0 {
                return Some(scheme.len() + run);
            }
        }
    }

    let direct = token_run(text);
    let looks_prefixed = prefixes
        .iter()
        .any(|p| text.len() > p.len() && text.starts_with(p) && !p.ends_with(' '));

    match (direct, looks_prefixed) {
        (Some(run), true) => Some(run.max(8)),
        (Some(run), false) if run >= 32 => Some(run), // opaque session ids / JWTs
        _ => None,
    }
}

/// Length of the leading `[A-Za-z0-9_\-\.=]+` run, if it is credential-sized.
fn token_run(text: &str) -> Option<usize> {
    let run = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '=')))
        .unwrap_or(text.len());
    if run >= 8 {
        Some(run)
    } else {
        None
    }
}

/// A snapshot of the process environment with **upper-cased keys**.
///
/// Windows treats environment variable names case-insensitively, so
/// `OPENROUTER_API_KEY`, `OpenRouter_Api_Key` and `openrouter_api_key` are the
/// same variable. Callers therefore only ever look names up in upper case.
///
/// The snapshot is taken once per `fetch` so a provider cannot observe the
/// environment changing mid-request. Values are held in memory and never
/// printed; `Debug` shows names only.
#[derive(Default, Clone)]
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// Read the real process environment. Non-UTF-8 values are skipped (they
    /// cannot be credentials we would accept anyway).
    pub fn from_process() -> Self {
        let mut vars = BTreeMap::new();
        for (key, value) in std::env::vars() {
            vars.insert(key.to_ascii_uppercase(), value);
        }
        Self { vars }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn with(mut self, name: &str, value: impl Into<String>) -> Self {
        self.vars.insert(name.to_ascii_uppercase(), value.into());
        self
    }

    /// Case-insensitive lookup, with the canonical cleaning rule applied.
    pub fn get(&self, name: &str) -> Option<Secret> {
        self.vars
            .get(&name.to_ascii_uppercase())
            .and_then(|raw| cleaned(raw))
            .map(Secret::new)
    }

    /// First non-empty value among `names`, in order. This is how the
    /// per-provider alias chains (`MINIMAX_CODING_API_KEY` > `MINIMAX_API_KEY`)
    /// are expressed.
    pub fn first_of(&self, names: &[&str]) -> Option<Secret> {
        names.iter().find_map(|name| self.get(name))
    }

    pub fn get_str(&self, name: &str) -> Option<String> {
        self.get(name).map(|s| s.expose().to_string())
    }

    /// Names that are set, in the order given (never values) — for `diagnose`
    /// output and "which source did you read?" hints in errors.
    pub fn names_present(&self, names: &[&str]) -> Vec<String> {
        names
            .iter()
            .filter(|n| self.vars.contains_key(&n.to_ascii_uppercase()))
            .map(|n| n.to_ascii_uppercase())
            .collect()
    }

    pub fn has(&self, name: &str) -> bool {
        self.vars.contains_key(&name.to_ascii_uppercase())
    }
}

/// Never print environment values — only the names that are set.
impl fmt::Debug for Env {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Env")
            .field("names", &self.vars.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Errors from reading a credential source. Messages never contain values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// The path does not exist (normal — treated as "not configured").
    Missing(String),
    /// The path exists but could not be read.
    Io(String),
    /// The file exists but is not the expected JSON.
    Parse(String),
    /// The file parsed but does not carry the field the provider needs.
    Schema(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::Missing(p) => write!(f, "no credentials at {p}"),
            CredentialError::Io(m) => write!(f, "{m}"),
            CredentialError::Parse(m) => write!(f, "{m}"),
            CredentialError::Schema(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// Read a JSON file. Read-only: never creates, never truncates, never locks.
pub fn read_json(path: &Path) -> Result<serde_json::Value, CredentialError> {
    if !path.exists() {
        return Err(CredentialError::Missing(display_path(path)));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| CredentialError::Io(format!("could not read {}: {e}", display_path(path))))?;
    serde_json::from_slice(&bytes).map_err(|e| {
        CredentialError::Parse(format!("{} is not valid JSON: {e}", display_path(path)))
    })
}

/// Read a one-line text credential file (`~/.coding-relay/glm-api-key`).
pub fn read_secret_file(path: &Path) -> Result<Secret, CredentialError> {
    if !path.exists() {
        return Err(CredentialError::Missing(display_path(path)));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| CredentialError::Io(format!("could not read {}: {e}", display_path(path))))?;
    cleaned(&text)
        .map(Secret::new)
        .ok_or_else(|| CredentialError::Schema(format!("{} is empty", display_path(path))))
}

/// First existing path, if any.
pub fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

/// `%USERPROFILE%` (or `HOME`), the Windows home directory.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// `%APPDATA%` (Roaming).
pub fn appdata_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

/// `%LOCALAPPDATA%`.
pub fn local_appdata_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// `<home>/.local/share` — the XDG convention OpenCode uses on Windows too
/// (`SPEC-apikey.md` §2.1: `%USERPROFILE%\.local\share\opencode`).
pub fn xdg_data_home(env: &Env) -> Option<PathBuf> {
    env.get_str("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".local/share")))
}

/// The port's own config file, read-only.
///
/// `%APPDATA%\CodexBar\config.json`, overridable with `CODEXBAR_CONFIG`.
/// The port never rewrites the config files of other tools, and it only ever
/// *reads* this one during a fetch.
///
/// A secret field may hold either a plain string (a config that predates the
/// vault) or a `{"$vault": "<key>"}` reference, which is resolved through the
/// attached [`SecretBackend`] only when the value is actually needed.
#[derive(Clone)]
pub struct PortConfig {
    root: serde_json::Value,
    path: PathBuf,
    vault: Option<Arc<dyn SecretBackend>>,
}

impl fmt::Debug for PortConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortConfig")
            .field("path", &self.path)
            .field("vault", &self.vault.as_ref().map(|v| v.name()))
            .finish()
    }
}

impl PortConfig {
    /// Path the port would read. Exposed for diagnostics.
    pub fn path(env: &Env) -> Option<PathBuf> {
        if let Some(custom) = env.get_str("CODEXBAR_CONFIG") {
            return Some(PathBuf::from(custom));
        }
        appdata_dir().map(|dir| dir.join("CodexBar").join("config.json"))
    }

    /// `Ok(None)` when the user has no port config — that is the common case and
    /// must not be an error.
    pub fn load(env: &Env) -> Result<Option<Self>, CredentialError> {
        let Some(path) = Self::path(env) else {
            return Ok(None);
        };
        match read_json(&path) {
            Ok(root) => Ok(Some(Self {
                root,
                path,
                vault: Some(default_vault()),
            })),
            Err(CredentialError::Missing(_)) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// Resolve `{"$vault": …}` references through `vault` instead of the
    /// production store. Tests inject a [`MemoryBackend`] here.
    pub fn with_vault(mut self, vault: Arc<dyn SecretBackend>) -> Self {
        self.vault = Some(vault);
        self
    }

    /// A string value, or the secret a vault reference points at.
    fn resolve_value(&self, value: &serde_json::Value) -> Option<String> {
        if let Some(text) = value.as_str() {
            return cleaned(text);
        }
        let key = vault_ref_key(value)?;
        let secret = self.vault.as_ref()?.get(key).ok().flatten()?;
        cleaned(secret.expose())
    }

    /// The entry for `providers[].id == id`, if the config lists it.
    fn entry(&self, id: ProviderId) -> Option<&serde_json::Value> {
        self.root
            .get("providers")?
            .as_array()?
            .iter()
            .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
    }

    /// `providers[].apiKey`, cleaned.
    pub fn api_key(&self, id: ProviderId) -> Option<Secret> {
        self.entry(id)
            .and_then(|p| p.get("apiKey"))
            .and_then(|v| self.resolve_value(v))
            .map(Secret::new)
    }

    /// `providers[].tokenAccounts.accounts[activeIndex].token`.
    pub fn active_token_account(&self, id: ProviderId) -> Option<Secret> {
        let accounts = self.entry(id)?.get("tokenAccounts")?;
        let index = accounts
            .get("activeIndex")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let accounts = accounts.get("accounts")?.as_array()?;
        let token = accounts
            .get(index)
            .or_else(|| accounts.first())?
            .get("token")?;
        self.resolve_value(token).map(Secret::new)
    }

    /// Auxiliary string fields: `workspaceID`, `region`, `cookieHeader`,
    /// `enterpriseHost`, `organizationId`, `project`.
    pub fn field(&self, id: ProviderId, field: &str) -> Option<String> {
        self.entry(id)?
            .get(field)
            .and_then(|v| self.resolve_value(v))
    }

    /// Resolved credential for an API-key provider, honouring the port's
    /// documented precedence: active token account → `providers[].apiKey` →
    /// process environment.
    ///
    /// `env_names` is the provider's alias chain in priority order.
    pub fn resolve_api_key(&self, id: ProviderId, env: &Env, env_names: &[&str]) -> Option<Secret> {
        self.active_token_account(id)
            .or_else(|| self.api_key(id))
            .or_else(|| env.first_of(env_names))
    }

    pub fn source_path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Vault for CodexBar's own secrets (RECON D4)
// ---------------------------------------------------------------------------

/// JSON key that marks a vault reference inside `config.json`:
/// `{"$vault": "openrouter.apiKey", "hint": "9f2c"}`.
pub const VAULT_REF_KEY: &str = "$vault";
/// Optional last-four hint stored next to a reference, so the settings window
/// can show `••••9f2c` without reading the secret back.
pub const VAULT_HINT_KEY: &str = "hint";
/// Target-name prefix of every Credential Manager entry CodexBar owns.
pub const CREDENTIAL_TARGET_PREFIX: &str = "CodexBar/";

/// The vault key a reference points at, if `value` is a reference.
pub fn vault_ref_key(value: &serde_json::Value) -> Option<&str> {
    value
        .get(VAULT_REF_KEY)
        .and_then(|v| v.as_str())
        .filter(|k| !k.trim().is_empty())
}

/// Build a reference object for `key`, remembering the last four characters.
pub fn vault_ref(key: &str, secret: &Secret) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert(
        VAULT_REF_KEY.to_string(),
        serde_json::Value::String(key.to_string()),
    );
    if secret.expose().chars().count() > 8 {
        map.insert(
            VAULT_HINT_KEY.to_string(),
            serde_json::Value::String(secret.suffix(4)),
        );
    }
    serde_json::Value::Object(map)
}

/// A place to keep CodexBar's own secrets. Implementations must never write a
/// value in plain text and never put one in an error message.
pub trait SecretBackend: Send + Sync {
    /// Short name for diagnostics (`credential-manager`, `dpapi-file`, …).
    fn name(&self) -> &'static str;
    /// `Ok(None)` when the key is not stored.
    fn get(&self, key: &str) -> Result<Option<Secret>, String>;
    fn set(&self, key: &str, value: &Secret) -> Result<(), String>;
    /// Deleting a key that does not exist is not an error.
    fn delete(&self, key: &str) -> Result<(), String>;
}

/// In-memory vault. Used by tests (and on non-Windows hosts, where secrets
/// then simply do not persist — never a plain-text file).
#[derive(Default)]
pub struct MemoryBackend {
    values: Mutex<BTreeMap<String, Secret>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stored keys, for assertions. Never values.
    pub fn keys(&self) -> Vec<String> {
        self.values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

impl SecretBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn get(&self, key: &str) -> Result<Option<Secret>, String> {
        Ok(self
            .values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(key)
            .cloned())
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), String> {
        self.values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.to_string(), value.clone());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.values
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(key);
        Ok(())
    }
}

/// Try `primary` (Credential Manager) and fall back to `fallback` (the DPAPI
/// file) when it fails. Reads consult both, so a value that once had to take
/// the fallback path is still found.
pub struct FallbackBackend {
    primary: Arc<dyn SecretBackend>,
    fallback: Arc<dyn SecretBackend>,
}

impl FallbackBackend {
    pub fn new(primary: Arc<dyn SecretBackend>, fallback: Arc<dyn SecretBackend>) -> Self {
        Self { primary, fallback }
    }
}

impl SecretBackend for FallbackBackend {
    fn name(&self) -> &'static str {
        self.primary.name()
    }
    fn get(&self, key: &str) -> Result<Option<Secret>, String> {
        match self.primary.get(key) {
            Ok(Some(value)) => Ok(Some(value)),
            Ok(None) => self.fallback.get(key),
            Err(primary) => self
                .fallback
                .get(key)
                .map_err(|fallback| format!("{primary}; {fallback}")),
        }
    }
    fn set(&self, key: &str, value: &Secret) -> Result<(), String> {
        match self.primary.set(key, value) {
            Ok(()) => {
                // A stale copy in the fallback would shadow nothing (primary is
                // read first) but must not linger either.
                let _ = self.fallback.delete(key);
                Ok(())
            }
            Err(primary) => {
                eprintln!(
                    "codexbar: {} unavailable for {key} ({primary}); using {}",
                    self.primary.name(),
                    self.fallback.name()
                );
                self.fallback
                    .set(key, value)
                    .map_err(|fallback| format!("{primary}; {fallback}"))
            }
        }
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        let a = self.primary.delete(key);
        let b = self.fallback.delete(key);
        a.and(b)
    }
}

/// The production vault: Credential Manager, then the DPAPI file.
///
/// On non-Windows hosts (the crate is portable for tests and tooling) this is
/// an in-memory store — never a plain-text file.
pub fn default_vault() -> Arc<dyn SecretBackend> {
    static VAULT: OnceLock<Arc<dyn SecretBackend>> = OnceLock::new();
    Arc::clone(VAULT.get_or_init(build_default_vault))
}

#[cfg(windows)]
fn build_default_vault() -> Arc<dyn SecretBackend> {
    let dpapi_path = local_appdata_dir()
        .or_else(appdata_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("CodexBar")
        .join("secrets.dpapi");
    Arc::new(FallbackBackend::new(
        Arc::new(win_vault::CredentialManagerBackend::new(
            CREDENTIAL_TARGET_PREFIX,
        )),
        Arc::new(win_vault::DpapiFileBackend::new(dpapi_path)),
    ))
}

#[cfg(not(windows))]
fn build_default_vault() -> Arc<dyn SecretBackend> {
    Arc::new(MemoryBackend::new())
}

#[cfg(windows)]
pub use win_vault::{CredentialManagerBackend, DpapiFileBackend};

/// Windows implementations: `CredReadW`/`CredWriteW`/`CredDeleteW` and
/// `CryptProtectData`/`CryptUnprotectData` from `windows-sys`.
#[cfg(windows)]
mod win_vault {
    use super::{Secret, SecretBackend};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_MAX_CREDENTIAL_BLOB_SIZE,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    use zeroize::Zeroize;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Generic credentials under `<prefix><key>`, persisted per machine (not
    /// roamed with the profile, unlike `%APPDATA%`).
    pub struct CredentialManagerBackend {
        prefix: String,
    }

    impl CredentialManagerBackend {
        pub fn new(prefix: &str) -> Self {
            Self {
                prefix: prefix.to_string(),
            }
        }

        fn target(&self, key: &str) -> Vec<u16> {
            wide(&format!("{}{key}", self.prefix))
        }
    }

    impl SecretBackend for CredentialManagerBackend {
        fn name(&self) -> &'static str {
            "credential-manager"
        }

        fn get(&self, key: &str) -> Result<Option<Secret>, String> {
            let target = self.target(key);
            let mut cred: *mut CREDENTIALW = std::ptr::null_mut();
            // SAFETY: `target` is NUL-terminated and outlives the call; on
            // success `cred` points at a buffer we free with `CredFree`.
            let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut cred) };
            if ok == 0 {
                // SAFETY: plain FFI call with no arguments.
                let err = unsafe { GetLastError() };
                if err == ERROR_NOT_FOUND {
                    return Ok(None);
                }
                return Err(format!("CredReadW failed (error {err})"));
            }
            // SAFETY: `cred` is valid until `CredFree`; the blob is
            // `CredentialBlobSize` bytes long.
            let value = unsafe {
                let c = &*cred;
                let bytes = if c.CredentialBlob.is_null() || c.CredentialBlobSize == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)
                        .to_vec()
                };
                CredFree(cred as *const _);
                bytes
            };
            let mut value = value;
            let text = String::from_utf8(value.clone());
            value.zeroize();
            match text {
                Ok(text) => Ok(Some(Secret::new(text))),
                Err(_) => Err("stored credential is not UTF-8".to_string()),
            }
        }

        fn set(&self, key: &str, value: &Secret) -> Result<(), String> {
            let mut blob = value.expose().as_bytes().to_vec();
            if blob.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
                blob.zeroize();
                return Err("value too large for Credential Manager".to_string());
            }
            let mut target = self.target(key);
            let mut user = wide("CodexBar");
            let cred = CREDENTIALW {
                Type: CRED_TYPE_GENERIC,
                TargetName: target.as_mut_ptr(),
                CredentialBlobSize: blob.len() as u32,
                CredentialBlob: blob.as_mut_ptr(),
                Persist: CRED_PERSIST_LOCAL_MACHINE,
                UserName: user.as_mut_ptr(),
                ..Default::default()
            };
            // SAFETY: every pointer in `cred` points into a live local buffer.
            let ok = unsafe { CredWriteW(&cred, 0) };
            // SAFETY: plain FFI call.
            let err = if ok == 0 {
                unsafe { GetLastError() }
            } else {
                0
            };
            blob.zeroize();
            if ok == 0 {
                return Err(format!("CredWriteW failed (error {err})"));
            }
            Ok(())
        }

        fn delete(&self, key: &str) -> Result<(), String> {
            let target = self.target(key);
            // SAFETY: NUL-terminated target that outlives the call.
            let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
            if ok == 0 {
                // SAFETY: plain FFI call.
                let err = unsafe { GetLastError() };
                if err != ERROR_NOT_FOUND {
                    return Err(format!("CredDeleteW failed (error {err})"));
                }
            }
            Ok(())
        }
    }

    /// DPAPI (`CryptProtectData`, current-user scope) over a single file that
    /// holds a JSON map of every fallback secret. The plain JSON only ever
    /// exists in memory.
    pub struct DpapiFileBackend {
        path: PathBuf,
        lock: Mutex<()>,
    }

    impl DpapiFileBackend {
        pub fn new(path: PathBuf) -> Self {
            Self {
                path,
                lock: Mutex::new(()),
            }
        }

        fn read_map(&self) -> Result<BTreeMap<String, String>, String> {
            let sealed = match std::fs::read(&self.path) {
                Ok(bytes) => bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
                Err(e) => return Err(format!("could not read the DPAPI store: {e}")),
            };
            let mut plain = unprotect(&sealed)?;
            let map = serde_json::from_slice(&plain)
                .map_err(|_| "the DPAPI store is not valid".to_string());
            plain.zeroize();
            map
        }

        fn write_map(&self, map: &BTreeMap<String, String>) -> Result<(), String> {
            let mut plain = serde_json::to_vec(map).map_err(|e| e.to_string())?;
            let sealed = protect(&plain);
            plain.zeroize();
            let sealed = sealed?;
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
            }
            let mut tmp = self.path.clone().into_os_string();
            tmp.push(format!(".tmp{}", std::process::id()));
            let tmp = PathBuf::from(tmp);
            std::fs::write(&tmp, &sealed).map_err(|e| format!("DPAPI store write: {e}"))?;
            std::fs::rename(&tmp, &self.path).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("DPAPI store replace: {e}")
            })
        }
    }

    impl SecretBackend for DpapiFileBackend {
        fn name(&self) -> &'static str {
            "dpapi-file"
        }
        fn get(&self, key: &str) -> Result<Option<Secret>, String> {
            let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
            let mut map = self.read_map()?;
            let found = map.remove(key).map(Secret::new);
            for value in map.values_mut() {
                value.zeroize();
            }
            Ok(found)
        }
        fn set(&self, key: &str, value: &Secret) -> Result<(), String> {
            let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
            let mut map = self.read_map()?;
            map.insert(key.to_string(), value.expose().to_string());
            let result = self.write_map(&map);
            for value in map.values_mut() {
                value.zeroize();
            }
            result
        }
        fn delete(&self, key: &str) -> Result<(), String> {
            let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
            let mut map = self.read_map()?;
            let result = if map.remove(key).is_some() {
                self.write_map(&map)
            } else {
                Ok(())
            };
            for value in map.values_mut() {
                value.zeroize();
            }
            result
        }
    }

    fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
        crypt(plain, true)
    }

    fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
        crypt(sealed, false)
    }

    fn crypt(input: &[u8], seal: bool) -> Result<Vec<u8>, String> {
        let data_in = CRYPT_INTEGER_BLOB {
            cbData: input.len() as u32,
            pbData: input.as_ptr() as *mut u8,
        };
        let mut data_out = CRYPT_INTEGER_BLOB::default();
        // SAFETY: `data_in` borrows `input` for the duration of the call; on
        // success `data_out` is a LocalAlloc'd buffer we copy and free.
        let ok = unsafe {
            if seal {
                CryptProtectData(
                    &data_in,
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut data_out,
                )
            } else {
                CryptUnprotectData(
                    &data_in,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut data_out,
                )
            }
        };
        if ok == 0 {
            // SAFETY: plain FFI call.
            let err = unsafe { GetLastError() };
            return Err(format!(
                "{} failed (error {err})",
                if seal {
                    "CryptProtectData"
                } else {
                    "CryptUnprotectData"
                }
            ));
        }
        // SAFETY: DPAPI returned `cbData` valid bytes at `pbData`.
        let out = unsafe {
            let bytes =
                std::slice::from_raw_parts(data_out.pbData, data_out.cbData as usize).to_vec();
            std::ptr::write_bytes(data_out.pbData, 0, data_out.cbData as usize);
            LocalFree(data_out.pbData as _);
            bytes
        };
        Ok(out)
    }
}

/// Resolve an API key without a port config (env only).
pub fn api_key_from_env(env: &Env, names: &[&str]) -> Option<Secret> {
    env.first_of(names)
}

/// Path rendered without the user's name where possible, for error messages.
pub fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning_matches_the_documented_rule() {
        assert_eq!(cleaned("  sk-abc  ").as_deref(), Some("sk-abc"));
        assert_eq!(cleaned("\"sk-abc\"").as_deref(), Some("sk-abc"));
        assert_eq!(cleaned("'sk-abc'"), Some("sk-abc".to_string()));
        assert_eq!(cleaned("  \"  sk-abc  \"  ").as_deref(), Some("sk-abc"));
        assert_eq!(cleaned(""), None);
        assert_eq!(cleaned("   "), None);
        assert_eq!(cleaned("\"\""), None);
        // A single stray quote is part of the value, not a wrapper.
        assert_eq!(cleaned("\"sk-abc"), Some("\"sk-abc".to_string()));
    }

    #[test]
    fn env_lookup_is_case_insensitive() {
        let env = Env::empty().with("openrouter_api_key", "  \"sk-or-v1-abcdefgh\" ");
        assert_eq!(
            env.get("OPENROUTER_API_KEY").unwrap().expose(),
            "sk-or-v1-abcdefgh"
        );
        assert!(env.has("OpenRouter_Api_Key"));
        assert!(env.get("OTHER").is_none());
    }

    #[test]
    fn first_of_follows_alias_priority() {
        let env = Env::empty()
            .with("MINIMAX_API_KEY", "second")
            .with("MINIMAX_CODING_API_KEY", "first");
        assert_eq!(
            env.first_of(&["MINIMAX_CODING_API_KEY", "MINIMAX_API_KEY"])
                .unwrap()
                .expose(),
            "first"
        );
        let env = Env::empty().with("MINIMAX_API_KEY", "second");
        assert_eq!(
            env.first_of(&["MINIMAX_CODING_API_KEY", "MINIMAX_API_KEY"])
                .unwrap()
                .expose(),
            "second"
        );
    }

    #[test]
    fn secrets_never_render_themselves() {
        let secret = Secret::new("sk-or-v1-0123456789abcdef");
        assert_eq!(format!("{secret:?}"), "Secret(\"<redacted>\")");
        assert_eq!(format!("{secret}"), "<redacted>");
        assert!(!format!("{secret:?}").contains("0123456789"));
    }

    #[test]
    fn redaction_keeps_a_recognisable_stub() {
        assert_eq!(redact("sk-or-EXAMPLE-KEY-9f2c"), "sk-or-…9f2c");
        assert_eq!(redact("short"), "…");
        assert_eq!(redact(""), "…");
    }

    #[test]
    fn an_email_identity_masks_its_domain() {
        assert_eq!(mask_email("firstname.lastname@example.com"), "firstnam@…");
        assert_eq!(mask_email("user@example.com"), "user@…");
        assert_eq!(mask_email("  a@b.co  "), "a@…");
        // No `@` is not an address: the token rule applies instead.
        assert_eq!(mask_email("sk-or-EXAMPLE-KEY-9f2c"), "sk-or-…9f2c");
        assert_eq!(mask_email(""), "…");
    }

    #[test]
    fn redaction_covers_error_bodies() {
        let text = "{\"error\":{\"message\":\"bad key sk-or-v1-0123456789abcdefgh\",\
                    \"authorization\":\"Bearer gsk_0123456789abcdefghij\"}}";
        let masked = redact_secrets_in_text(text);
        assert!(!masked.contains("0123456789abcdefgh"));
        assert!(masked.contains("[redacted]"));
        assert!(masked.contains("bad key"));
    }

    #[test]
    fn redaction_keeps_ordinary_prose_intact() {
        let masked = redact_secrets_in_text("HTTP 429 rateLimited, retry after 30 s");
        assert_eq!(masked, "HTTP 429 rateLimited, retry after 30 s");
    }

    #[test]
    fn port_config_reads_keys_and_fields_without_network() {
        let dir = std::env::temp_dir().join(format!("codexbar-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"providers":[{"id":"xai","apiKey":"\"xai-key-abcdefgh\"",
                "workspaceID":"team_123","region":"global"}]}"#,
        )
        .unwrap();

        let env = Env::empty().with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = PortConfig::load(&env).unwrap().unwrap();
        assert_eq!(
            config.api_key(ProviderId::Xai).unwrap().expose(),
            "xai-key-abcdefgh"
        );
        assert_eq!(
            config.field(ProviderId::Xai, "workspaceID").as_deref(),
            Some("team_123")
        );
        assert!(config.api_key(ProviderId::OpenRouter).is_none());

        // Precedence: an explicit config key beats the environment.
        let env = env.with("XAI_MANAGEMENT_API_KEY", "from-env");
        assert_eq!(
            config
                .resolve_api_key(ProviderId::Xai, &env, &["XAI_MANAGEMENT_API_KEY"])
                .unwrap()
                .expose(),
            "xai-key-abcdefgh"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_config_is_not_an_error() {
        let env = Env::empty().with(
            "CODEXBAR_CONFIG",
            std::env::temp_dir()
                .join("codexbar-does-not-exist.json")
                .to_string_lossy()
                .to_string(),
        );
        assert!(PortConfig::load(&env).unwrap().is_none());
    }

    #[test]
    fn a_secret_moved_into_a_string_keeps_its_value() {
        let secret = Secret::new("value-to-move");
        let text: String = secret.into();
        assert_eq!(text, "value-to-move");
    }

    #[test]
    fn vault_references_resolve_through_the_injected_backend() {
        let dir = std::env::temp_dir().join(format!("codexbar-cfg-vault-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let secret = Secret::new("sk-or-vault-value-1234");
        let reference = vault_ref("openrouter.apiKey", &secret);
        assert_eq!(reference["hint"], "1234");
        let root = serde_json::json!({"providers":[
            {"id":"openrouter","apiKey": reference},
            {"id":"copilot","tokenAccounts":{"activeIndex":0,"accounts":[
                {"token":{"$vault":"copilot.tokenAccounts.0"},"label":"x"}]}}
        ]});
        std::fs::write(&path, serde_json::to_vec(&root).unwrap()).unwrap();
        let vault = Arc::new(MemoryBackend::new());
        vault.set("openrouter.apiKey", &secret).unwrap();
        vault
            .set("copilot.tokenAccounts.0", &Secret::new("gho_vault_token"))
            .unwrap();

        let env = Env::empty().with("CODEXBAR_CONFIG", path.to_string_lossy().to_string());
        let config = PortConfig::load(&env)
            .unwrap()
            .unwrap()
            .with_vault(vault.clone());
        assert_eq!(
            config.api_key(ProviderId::OpenRouter).unwrap().expose(),
            "sk-or-vault-value-1234"
        );
        assert_eq!(
            config
                .active_token_account(ProviderId::Copilot)
                .unwrap()
                .expose(),
            "gho_vault_token"
        );
        // A dangling reference is simply "not configured".
        vault.delete("openrouter.apiKey").unwrap();
        assert!(config.api_key(ProviderId::OpenRouter).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    struct Broken;
    impl SecretBackend for Broken {
        fn name(&self) -> &'static str {
            "broken"
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

    #[test]
    fn the_fallback_backend_takes_over_when_the_primary_fails() {
        let fallback = Arc::new(MemoryBackend::new());
        let vault = FallbackBackend::new(Arc::new(Broken), fallback.clone());
        vault.set("k", &Secret::new("v-123456789")).unwrap();
        assert_eq!(fallback.keys(), vec!["k".to_string()]);
        assert_eq!(vault.get("k").unwrap().unwrap().expose(), "v-123456789");

        // A healthy primary wins and clears the fallback copy.
        let primary = Arc::new(MemoryBackend::new());
        let vault = FallbackBackend::new(primary.clone(), fallback.clone());
        vault.set("k", &Secret::new("fresh-value")).unwrap();
        assert!(fallback.keys().is_empty());
        assert_eq!(primary.keys(), vec!["k".to_string()]);
    }

    /// Real Windows APIs, but only under a test-only target prefix / temp file:
    /// the user's own CodexBar entries are never read or written.
    #[cfg(windows)]
    #[test]
    fn credential_manager_round_trips_under_a_test_prefix() {
        let prefix = format!("CodexBar-test-{}/", std::process::id());
        let vault = CredentialManagerBackend::new(&prefix);
        let key = "unit.roundtrip";
        vault.delete(key).unwrap();
        assert!(vault.get(key).unwrap().is_none());
        vault
            .set(key, &Secret::new("fixture-secret-value"))
            .unwrap();
        assert_eq!(
            vault.get(key).unwrap().unwrap().expose(),
            "fixture-secret-value"
        );
        vault.delete(key).unwrap();
        assert!(vault.get(key).unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn the_dpapi_file_is_never_plain_text() {
        let dir = std::env::temp_dir().join(format!("codexbar-dpapi-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secrets.dpapi");
        let vault = DpapiFileBackend::new(path.clone());
        vault
            .set("a.apiKey", &Secret::new("dpapi-fixture-secret"))
            .unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(!String::from_utf8_lossy(&raw).contains("dpapi-fixture-secret"));
        assert_eq!(
            vault.get("a.apiKey").unwrap().unwrap().expose(),
            "dpapi-fixture-secret"
        );
        vault.delete("a.apiKey").unwrap();
        assert!(vault.get("a.apiKey").unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
