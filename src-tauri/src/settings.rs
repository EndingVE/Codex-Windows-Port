//! Persistent settings for the Windows port.
//!
//! Stored as camelCase JSON at `%APPDATA%\CodexBar\config.json` (the macOS app
//! keeps its preferences in `UserDefaults`; on Windows a plain file is the
//! closest equivalent that stays inspectable and scriptable).
//!
//! Rules:
//!  * every field is `#[serde(default)]` — an old or hand-edited file never
//!    fails to load, it is simply normalised;
//!  * writes are atomic (`config.json.tmp` → rename) so a crash mid-write can
//!    never leave a truncated file behind;
//!  * [`Settings::normalized`] clamps the refresh interval and reconciles the
//!    provider list against [`ProviderId::ALL`] (order + enabled flags).

use std::fs;
use std::path::{Path, PathBuf};

use codexbar_core::ProviderId;
use serde::{Deserialize, Serialize};

/// Directory under `%APPDATA%`.
pub const APP_DIR: &str = "CodexBar";
/// File name inside [`APP_DIR`].
pub const CONFIG_FILE: &str = "config.json";
/// Lower bound for the background refresh tick.
pub const MIN_REFRESH_SECS: u64 = 15;
/// Upper bound for the background refresh tick (a day).
pub const MAX_REFRESH_SECS: u64 = 86_400;
/// Default cadence: five minutes, like the macOS app's idle poll.
pub const DEFAULT_REFRESH_SECS: u64 = 300;

fn default_true() -> bool {
    true
}

/// One provider entry: which provider, and whether it is shown in the tray.
///
/// Order comes from the position in [`Settings::providers`], which is why this
/// is a list and not a map.
///
/// `extra` carries every other field the entry has in the file — notably the
/// credentials the provider crate reads (`apiKey`,
/// `tokenAccounts`, `workspaceID`, …). They are round-tripped verbatim so that a
/// save from the settings window can never silently drop a stored credential.
/// See `token_store.rs` for the writer that fills `tokenAccounts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderPref {
    /// Machine id — matches the JSON `provider` field (`codex`, `claude`, …).
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Every other key of this entry, preserved byte-for-byte.
    #[serde(flatten, default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ProviderPref {
    pub fn new(id: &str, enabled: bool) -> Self {
        Self {
            id: id.to_string(),
            enabled,
            extra: serde_json::Map::new(),
        }
    }
}

/// Everything the settings window edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Enabled providers and their tray/display order.
    pub providers: Vec<ProviderPref>,
    /// Background refresh cadence, seconds.
    pub refresh_interval_secs: u64,
    /// Register the app under `HKCU\...\CurrentVersion\Run`.
    pub start_at_login: bool,
    /// One tray icon for everything (with a provider selector) instead of one
    /// icon per provider.
    pub merge_icons: bool,
    /// Let providers touch the keychain/credential store while refreshing.
    /// Off by default: the port never reads credentials unless asked to.
    pub refresh_credentials: bool,
    /// Serve the deterministic **mock** registry instead of the live one.
    ///
    /// Off by default. When off, the tray app reads this machine's local
    /// credentials and fetches real usage exactly like `codexbar usage --live`
    /// (the two share `codexbar_providers::live_registry()`). When on, no
    /// credential is read and no socket is opened — every card is sample data
    /// and the popover shows its "sample data" banner.
    ///
    /// There is deliberately **no automatic fallback**: a machine with zero
    /// credentials still gets the live registry, where each provider honestly
    /// reports `notConfigured` instead of inventing numbers.
    pub mock_mode: bool,
    /// Draw the rounded percentage into each tray icon.
    pub show_percent_in_icon: bool,
    /// Which provider the merged icon mirrors; `None` = worst of all enabled.
    pub merged_provider: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            providers: ProviderId::ALL
                .iter()
                .map(|id| ProviderPref::new(id.as_str(), true))
                .collect(),
            refresh_interval_secs: DEFAULT_REFRESH_SECS,
            start_at_login: false,
            merge_icons: false,
            refresh_credentials: false,
            mock_mode: false,
            show_percent_in_icon: true,
            merged_provider: None,
        }
    }
}

impl Settings {
    /// Clamp and reconcile so the rest of the app can trust the value.
    pub fn normalized(mut self) -> Self {
        self.refresh_interval_secs = self
            .refresh_interval_secs
            .clamp(MIN_REFRESH_SECS, MAX_REFRESH_SECS);

        // Keep known providers only, de-duplicated, in the user's order…
        let mut seen: Vec<String> = Vec::new();
        let mut ordered: Vec<ProviderPref> = Vec::new();
        for pref in self.providers.drain(..) {
            let Some(id) = ProviderId::from_str_lossy(&pref.id) else {
                continue;
            };
            let key = id.as_str().to_string();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            ordered.push(ProviderPref {
                id: key,
                enabled: pref.enabled,
                extra: pref.extra,
            });
        }
        // …then append anything the file did not mention (new providers land
        // enabled, which is what a fresh install wants).
        for id in ProviderId::ALL {
            let key = id.as_str().to_string();
            if !seen.contains(&key) {
                ordered.push(ProviderPref {
                    id: key,
                    enabled: true,
                    extra: serde_json::Map::new(),
                });
            }
        }
        self.providers = ordered;

        if let Some(selected) = self.merged_provider.clone() {
            self.merged_provider =
                ProviderId::from_str_lossy(&selected).map(|p| p.as_str().to_string());
        }
        self
    }

    /// Enabled providers in display order.
    pub fn enabled_providers(&self) -> Vec<ProviderId> {
        self.providers
            .iter()
            .filter(|p| p.enabled)
            .filter_map(|p| ProviderId::from_str_lossy(&p.id))
            .collect()
    }

    /// The copy the webview may see (RECON D1): every secret in `extra`
    /// becomes `{"hasKey": true, "masked": "••••1234"}`. Sending it back
    /// through [`save_merged`] leaves the stored secrets untouched.
    pub fn for_webview(&self) -> Settings {
        let mut out = self.clone();
        for pref in &mut out.providers {
            crate::secret_store::redact_entry(&mut pref.extra);
        }
        out
    }
}

/// `%APPDATA%\CodexBar`, falling back to the executable's directory when the
/// environment variable is missing (rare, but keeps us from panicking).
pub fn config_dir() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        if !appdata.trim().is_empty() {
            return PathBuf::from(appdata).join(APP_DIR);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

/// Full path of the settings file.
pub fn config_path() -> PathBuf {
    config_dir().join(CONFIG_FILE)
}

/// Read the settings file. A missing or unreadable file yields defaults — the
/// app must always be able to start.
///
/// Plain-text secrets left by an older build are first moved to the vault
/// ([`crate::secret_store::migrate_config_file`]); the migration is idempotent
/// and a no-op once done.
pub fn load() -> Settings {
    load_from(&config_path())
}

/// [`load`] for an explicit path (tests use a temp dir).
///
/// The returned value is already [`Settings::for_webview`]-redacted: the
/// in-memory state never holds a secret or a vault reference, and saving it
/// back through [`save_merged`] leaves every stored secret as it is.
pub fn load_from(path: &Path) -> Settings {
    let vault = crate::secret_store::vault_for(path);
    match crate::secret_store::migrate_config_file(path, vault.as_ref()) {
        Ok(report) if report.changed() => eprintln!(
            "codexbar: moved {} secret(s) from {} to {} ({} from the backup)",
            report.moved,
            path.display(),
            vault.name(),
            report.backup_moved
        ),
        Ok(_) => {}
        Err(err) => eprintln!("codexbar: secret migration skipped: {err}"),
    }
    read_settings(path).for_webview()
}

fn read_settings(path: &Path) -> Settings {
    match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => settings.normalized(),
            Err(err) => {
                eprintln!(
                    "codexbar: {} is not valid JSON ({err}), using defaults",
                    path.display()
                );
                Settings::default()
            }
        },
        Err(_) => Settings::default(),
    }
}

/// Persist settings. Kept for existing callers; it is [`save_merged`], so no
/// caller can clobber a stored secret.
pub fn save(settings: &Settings) -> Result<PathBuf, String> {
    save_merged(settings)
}

/// Merge `settings` over what is on disk and write it atomically (RECON D2/D3).
///
/// Under the process-wide [`crate::secret_store::config_lock`] (shared with
/// the device-flow token store) the file is re-read, `settings` is merged over
/// it — a secret that is empty, missing or masked means *unchanged*, a new one
/// goes to the vault — and the result replaces the file via temp + rename.
pub fn save_merged(settings: &Settings) -> Result<PathBuf, String> {
    let path = config_path();
    let vault = crate::secret_store::vault_for(&path);
    save_merged_to(&path, settings, vault.as_ref())?;
    Ok(path)
}

/// [`save_merged`] for an explicit path and vault (tests).
pub fn save_merged_to(
    path: &Path,
    settings: &Settings,
    vault: &dyn codexbar_providers::credential::SecretBackend,
) -> Result<(), String> {
    let _guard = crate::secret_store::config_lock();
    // Never merge over plain text: an older build's secrets move first.
    crate::secret_store::migrate_config_file_locked(path, vault)?;
    let disk: serde_json::Value = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "{} is not valid JSON, not overwriting it: {e}",
                path.display()
            )
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let incoming =
        serde_json::to_value(settings).map_err(|e| format!("could not serialise settings: {e}"))?;
    let merged = crate::secret_store::merge_root(&disk, &incoming, vault)?;
    crate::secret_store::write_atomic(path, &merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_enable_every_provider() {
        let s = Settings::default();
        assert_eq!(s.enabled_providers().len(), ProviderId::ALL.len());
        assert_eq!(s.refresh_interval_secs, DEFAULT_REFRESH_SECS);
        assert!(s.show_percent_in_icon);
        assert!(!s.merge_icons);
    }

    #[test]
    fn partial_json_falls_back_to_defaults_per_field() {
        let s: Settings = serde_json::from_str(r#"{"mergeIcons":true}"#).unwrap();
        assert!(s.merge_icons);
        assert_eq!(s.refresh_interval_secs, DEFAULT_REFRESH_SECS);
        assert_eq!(s.providers.len(), ProviderId::ALL.len());
    }

    #[test]
    fn normalizes_interval_and_provider_list() {
        let raw = Settings {
            refresh_interval_secs: 1,
            providers: vec![
                ProviderPref::new("gemini", false),
                ProviderPref::new("nope", true),
                ProviderPref::new("gemini", true),
            ],
            merged_provider: Some("nope".into()),
            ..Settings::default()
        }
        .normalized();
        assert_eq!(raw.refresh_interval_secs, MIN_REFRESH_SECS);
        assert_eq!(raw.providers[0].id, "gemini");
        assert_eq!(raw.providers.len(), ProviderId::ALL.len());
        // gemini appears once and keeps the first enabled flag (false).
        assert!(!raw.enabled_providers().contains(&ProviderId::Gemini));
        assert!(raw.merged_provider.is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let s = Settings {
            merge_icons: true,
            merged_provider: Some("codex".into()),
            ..Settings::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        // camelCase on the wire.
        assert!(text.contains("refreshIntervalSecs"));
        assert!(text.contains("mergeIcons"));
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back, s);
    }

    /// Live is the default: the app only serves sample data when the user asked
    /// for it explicitly (`mockMode: true`).
    #[test]
    fn mock_mode_is_off_by_default_and_round_trips() {
        let defaults = Settings::default();
        assert!(!defaults.mock_mode);
        // A stored file that has never seen the field keeps the live default…
        let parsed: Settings = serde_json::from_str(r#"{"mergeIcons":true}"#).unwrap();
        assert!(!parsed.mock_mode);
        // …and asking for it explicitly sticks, in camelCase.
        let asked = Settings {
            mock_mode: true,
            ..Settings::default()
        };
        let text = serde_json::to_string(&asked).unwrap();
        assert!(text.contains("\"mockMode\":true"));
        assert!(serde_json::from_str::<Settings>(&text).unwrap().mock_mode);
    }

    /// A save must never drop a provider's stored credential: `apiKey` and
    /// `tokenAccounts` are round-tripped verbatim (see `token_store.rs`).
    #[test]
    fn provider_credentials_survive_a_settings_round_trip() {
        let raw = r#"{"providers":[
            {"id":"copilot","enabled":true,"tokenAccounts":{"activeIndex":0,
              "accounts":[{"token":"gho-keep","label":"lbl"}]}},
            {"id":"openrouter","enabled":false,"apiKey":"sk-or-keep","customField":7}
          ]}"#;
        let settings: Settings = serde_json::from_str(raw).unwrap();
        let settings = settings.normalized();

        let text = serde_json::to_string(&settings).unwrap();
        assert!(text.contains("gho-keep"), "{text}");
        assert!(text.contains("sk-or-keep"), "{text}");
        assert!(text.contains("customField"), "{text}");

        let copilot = settings
            .providers
            .iter()
            .find(|p| p.id == "copilot")
            .unwrap();
        assert_eq!(
            copilot.extra["tokenAccounts"]["accounts"][0]["token"],
            "gho-keep"
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codexbar-settings-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.join(CONFIG_FILE)
    }

    const WITH_SECRETS: &str = r#"{"providers":[
        {"id":"copilot","enabled":true,"tokenAccounts":{"activeIndex":0,
          "accounts":[{"token":"gho-keep-fixture-token","label":"lbl"}]}},
        {"id":"openrouter","enabled":false,"apiKey":"sk-or-keep-fixture-9f2c","customField":7}
      ]}"#;

    /// (a) RECON D1: what `get_settings` hands the webview has no secret —
    /// neither the value nor the vault key — only presence and a masked hint.
    #[test]
    fn get_settings_never_returns_a_secret() {
        let path = scratch("webview");
        fs::write(&path, WITH_SECRETS).unwrap();

        // Both paths the command can take: a loaded state, and a state that
        // still carries raw values (defence in depth).
        let loaded = load_from(&path);
        let raw: Settings = serde_json::from_str(WITH_SECRETS).unwrap();
        for settings in [loaded.for_webview(), raw.normalized().for_webview()] {
            let text = serde_json::to_string(&settings).unwrap();
            assert!(!text.contains("gho-keep"), "{text}");
            assert!(!text.contains("sk-or-keep"), "{text}");
            assert!(!text.contains("$vault"), "{text}");
            assert!(text.contains("\"hasKey\":true"), "{text}");
            assert!(text.contains("••••9f2c"), "{text}");
            assert!(text.contains("customField"), "{text}");
        }
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    fn secret_at(vault: &dyn codexbar_providers::credential::SecretBackend, key: &str) -> String {
        vault.get(key).unwrap().unwrap().expose().to_string()
    }

    /// (b) RECON D2: saving what the settings window sends — providers with no
    /// `extra`, or with masked / empty secrets — keeps every stored secret.
    #[test]
    fn saving_with_empty_or_missing_secrets_keeps_them() {
        let path = scratch("merge");
        fs::write(&path, WITH_SECRETS).unwrap();
        let vault = codexbar_providers::credential::MemoryBackend::new();

        // What `ui/settings.js` sends: id + enabled only.
        let from_ui = Settings {
            refresh_interval_secs: 600,
            ..Settings::default()
        };
        save_merged_to(&path, &from_ui, &vault).unwrap();

        // An echo of `get_settings` (masked), plus an explicit empty key.
        let mut echo = load_from(&path).for_webview();
        for pref in &mut echo.providers {
            if pref.id == "openrouter" {
                pref.extra
                    .insert("apiKey".into(), serde_json::Value::String(String::new()));
            }
        }
        save_merged_to(&path, &echo, &vault).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("sk-or-keep"), "plain text on disk: {text}");
        assert!(!text.contains("gho-keep"), "plain text on disk: {text}");
        let root: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(root["refreshIntervalSecs"], 600);
        let entry = |id: &str| {
            root["providers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(entry("openrouter")["customField"], 7);
        assert_eq!(entry("openrouter")["apiKey"]["$vault"], "openrouter.apiKey");
        assert_eq!(
            secret_at(&vault, "openrouter.apiKey"),
            "sk-or-keep-fixture-9f2c"
        );
        assert_eq!(
            secret_at(&vault, "copilot.tokenAccounts.0"),
            "gho-keep-fixture-token"
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// RECON D3: a device-flow token stored *after* the tray loaded its state
    /// survives the tray's next save of that stale state.
    #[test]
    fn a_stale_tray_save_after_login_keeps_the_new_token() {
        let path = scratch("race");
        fs::write(&path, r#"{"providers":[{"id":"copilot","enabled":true}]}"#).unwrap();
        let vault = codexbar_providers::credential::MemoryBackend::new();
        let stale = load_from(&path);

        crate::token_store::store_device_token_in(
            &path,
            "copilot",
            &codexbar_providers::Secret::new("gho_fresh_fixture_token"),
            "lbl",
            &vault,
        )
        .unwrap();

        let mut next = stale.clone();
        next.merge_icons = true;
        save_merged_to(&path, &next.normalized(), &vault).unwrap();

        let root: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let copilot = root["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "copilot")
            .unwrap();
        assert_eq!(
            copilot["tokenAccounts"]["accounts"][0]["token"]["$vault"],
            "copilot.tokenAccounts.0"
        );
        assert_eq!(root["mergeIcons"], true);
        assert_eq!(
            secret_at(&vault, "copilot.tokenAccounts.0"),
            "gho_fresh_fixture_token"
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /// A config the merge cannot parse is never overwritten.
    #[test]
    fn a_save_never_clobbers_an_unparseable_config() {
        let path = scratch("corrupt");
        fs::write(&path, "{ not json").unwrap();
        let vault = codexbar_providers::credential::MemoryBackend::new();
        assert!(save_merged_to(&path, &Settings::default(), &vault).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not json");
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
