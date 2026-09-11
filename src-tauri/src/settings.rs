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
use std::path::PathBuf;

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
pub fn load() -> Settings {
    let path = config_path();
    match fs::read_to_string(&path) {
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

/// Write the settings atomically (temp file + rename) and return the path used.
pub fn save(settings: &Settings) -> Result<PathBuf, String> {
    let dir = config_dir();
    let path = dir.join(CONFIG_FILE);
    fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;

    let json = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("could not serialise settings: {e}"))?;

    let tmp = dir.join(format!("{CONFIG_FILE}.tmp"));
    fs::write(&tmp, json.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("could not replace {}: {e}", path.display())
    })?;
    Ok(path)
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
}
