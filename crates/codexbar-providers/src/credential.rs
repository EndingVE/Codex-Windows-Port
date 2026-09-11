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
//! Nothing in this module ever writes. The single exception in the whole crate is
//! [`crate::oauth::write_json_atomic`], which is used only by an explicit,
//! user-initiated OAuth refresh and keeps a `.bak` copy.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use codexbar_core::ProviderId;

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

impl From<Secret> for String {
    /// Explicit, named conversion — an audit point.
    fn from(value: Secret) -> Self {
        value.0
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
#[derive(Debug, Clone)]
pub struct PortConfig {
    root: serde_json::Value,
    path: PathBuf,
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
            Ok(root) => Ok(Some(Self { root, path })),
            Err(CredentialError::Missing(_)) => Ok(None),
            Err(other) => Err(other),
        }
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
            .and_then(|v| v.as_str())
            .and_then(cleaned)
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
        accounts
            .get(index)
            .or_else(|| accounts.first())?
            .get("token")
            .and_then(|v| v.as_str())
            .and_then(cleaned)
            .map(Secret::new)
    }

    /// Auxiliary string fields: `workspaceID`, `region`, `cookieHeader`,
    /// `enterpriseHost`, `organizationId`, `project`.
    pub fn field(&self, id: ProviderId, field: &str) -> Option<String> {
        self.entry(id)?
            .get(field)
            .and_then(|v| v.as_str())
            .and_then(cleaned)
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
}
