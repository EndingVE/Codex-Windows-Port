//! User-editable appearance: theme, accent, font scale, density and opacity.
//!
//! Stored as camelCase JSON at `%APPDATA%\CodexBar\appearance.json`, next to
//! `config.json` but deliberately in its own file: it is presentation only, it
//! never carries a secret, and the settings window can save it on every slider
//! tick without racing the provider settings.
//!
//! Robustness rules (same spirit as `settings.rs`):
//!  * a missing, unreadable or corrupt file yields [`Appearance::default`] — the
//!    app must always be able to start;
//!  * every field is parsed on its own, so one bad value (`"fontScale": "big"`)
//!    falls back to that field's default instead of discarding the whole file;
//!  * numeric fields are clamped to their documented range and the accent is
//!    normalised to lowercase `#rrggbb`;
//!  * writes are atomic (`appearance.json.tmp` → rename).
//!
//! The webview reads it through [`get_appearance`] and writes it through
//! [`set_appearance`], which persists, repaints the tray icons with the new
//! palette and broadcasts `appearance-changed` to every open window (popover,
//! settings and — once it exists — the widget).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{AppHandle, Emitter};

use crate::tray_icon::{self, TrayPalette};

/// File name under the settings directory.
pub const APPEARANCE_FILE: &str = "appearance.json";
/// Event broadcast to every webview after a successful `set_appearance`.
pub const APPEARANCE_EVENT: &str = "appearance-changed";
/// On-disk schema version.
pub const APPEARANCE_VERSION: u32 = 1;

/// Default accent (the historical `--accent` in `ui/styles.css`).
pub const DEFAULT_ACCENT: &str = "#6ea8fe";
/// Font / UI scale range, in percent.
pub const FONT_SCALE_MIN: u32 = 90;
pub const FONT_SCALE_MAX: u32 = 130;
pub const FONT_SCALE_DEFAULT: u32 = 100;
/// Opacity range for the popover and the widget (1.0 = opaque).
pub const OPACITY_MIN: f64 = 0.6;
pub const OPACITY_MAX: f64 = 1.0;

/// Theme preference. `System` follows `prefers-color-scheme` in the webviews
/// and the taskbar theme for the tray icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }
}

/// Spacing preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Normal,
}

impl Density {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "compact" => Some(Self::Compact),
            "normal" | "comfortable" => Some(Self::Normal),
            _ => None,
        }
    }
}

/// The persisted appearance. Field names are the JSON contract with
/// `ui/theme.js` (`window.CodexBarTheme.DEFAULTS` mirrors [`Default`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Appearance {
    pub version: u32,
    pub theme: ThemeMode,
    /// Lowercase `#rrggbb`.
    pub accent: String,
    /// Percent, `FONT_SCALE_MIN..=FONT_SCALE_MAX`.
    pub font_scale: u32,
    pub density: Density,
    /// `OPACITY_MIN..=OPACITY_MAX`.
    pub popover_opacity: f64,
    /// `OPACITY_MIN..=OPACITY_MAX`.
    pub widget_opacity: f64,
    /// Colour legend under the cards (migrated from the webview localStorage).
    pub show_legend: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            version: APPEARANCE_VERSION,
            theme: ThemeMode::System,
            accent: DEFAULT_ACCENT.to_string(),
            font_scale: FONT_SCALE_DEFAULT,
            density: Density::Normal,
            popover_opacity: OPACITY_MAX,
            widget_opacity: OPACITY_MAX,
            show_legend: true,
        }
    }
}

/// Normalise `#rgb` / `#rrggbb` (with or without `#`) to lowercase `#rrggbb`.
pub fn normalize_hex(raw: &str) -> Option<String> {
    let hex = raw.trim().trim_start_matches('#');
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let full = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => hex.to_string(),
        _ => return None,
    };
    Some(format!("#{}", full.to_ascii_lowercase()))
}

/// `#rrggbb` → `[r, g, b]`.
pub fn hex_to_rgb(hex: &str) -> Option<[u8; 3]> {
    let norm = normalize_hex(hex)?;
    let byte = |i: usize| u8::from_str_radix(&norm[i..i + 2], 16).ok();
    Some([byte(1)?, byte(3)?, byte(5)?])
}

fn clamp_opacity(value: f64) -> f64 {
    if value.is_finite() {
        // Two decimals are plenty and keep the JSON tidy.
        (value.clamp(OPACITY_MIN, OPACITY_MAX) * 100.0).round() / 100.0
    } else {
        OPACITY_MAX
    }
}

/// Accept `0.8` or `80` (percent) for opacities.
fn opacity_from(value: &Value) -> Option<f64> {
    let raw = value.as_f64()?;
    Some(clamp_opacity(if raw > 1.0 { raw / 100.0 } else { raw }))
}

fn font_scale_from(value: &Value) -> Option<u32> {
    let raw = value.as_f64()?;
    if !raw.is_finite() {
        return None;
    }
    // Accept a factor (1.1) as well as a percent (110).
    let pct = if raw <= 3.0 { raw * 100.0 } else { raw };
    Some((pct.round() as i64).clamp(FONT_SCALE_MIN as i64, FONT_SCALE_MAX as i64) as u32)
}

impl Appearance {
    /// Overlay every recognised, valid field of `patch` onto `self`.
    ///
    /// Unknown keys are ignored; an invalid value leaves the field unchanged,
    /// out-of-range numbers are clamped.
    pub fn merged_with(&self, patch: &Value) -> Self {
        let mut next = self.clone();
        let Some(obj) = patch.as_object() else {
            return next;
        };
        if let Some(mode) = obj
            .get("theme")
            .and_then(Value::as_str)
            .and_then(ThemeMode::parse)
        {
            next.theme = mode;
        }
        if let Some(accent) = obj
            .get("accent")
            .and_then(Value::as_str)
            .and_then(normalize_hex)
        {
            next.accent = accent;
        }
        if let Some(scale) = obj.get("fontScale").and_then(font_scale_from) {
            next.font_scale = scale;
        }
        if let Some(density) = obj
            .get("density")
            .and_then(Value::as_str)
            .and_then(Density::parse)
        {
            next.density = density;
        }
        if let Some(op) = obj.get("popoverOpacity").and_then(opacity_from) {
            next.popover_opacity = op;
        }
        if let Some(op) = obj.get("widgetOpacity").and_then(opacity_from) {
            next.widget_opacity = op;
        }
        if let Some(flag) = obj.get("showLegend").and_then(Value::as_bool) {
            next.show_legend = flag;
        }
        next.normalized()
    }

    /// Parse a JSON document; anything that is not an object gives defaults.
    pub fn from_json(text: &str) -> Self {
        match serde_json::from_str::<Value>(text) {
            Ok(value) if value.is_object() => Self::default().merged_with(&value),
            _ => Self::default(),
        }
    }

    /// Clamp / canonicalise every field.
    pub fn normalized(mut self) -> Self {
        self.version = APPEARANCE_VERSION;
        self.accent = normalize_hex(&self.accent).unwrap_or_else(|| DEFAULT_ACCENT.to_string());
        self.font_scale = self.font_scale.clamp(FONT_SCALE_MIN, FONT_SCALE_MAX);
        self.popover_opacity = clamp_opacity(self.popover_opacity);
        self.widget_opacity = clamp_opacity(self.widget_opacity);
        self
    }

    /// Whether the tray should draw its light palette.
    pub fn tray_is_light(&self, system_light: Option<bool>) -> bool {
        match self.theme {
            ThemeMode::Light => true,
            ThemeMode::Dark => false,
            ThemeMode::System => system_light.unwrap_or(false),
        }
    }

    /// Tray palette for this appearance.
    pub fn tray_palette(&self, system_light: Option<bool>) -> TrayPalette {
        let base = if self.tray_is_light(system_light) {
            TrayPalette::LIGHT
        } else {
            TrayPalette::DARK
        };
        let accent = hex_to_rgb(&self.accent).unwrap_or([110, 168, 254]);
        base.with_accent(accent)
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// `%APPDATA%\CodexBar\appearance.json`.
pub fn appearance_path() -> PathBuf {
    crate::settings::config_dir().join(APPEARANCE_FILE)
}

/// Read `path`; missing / unreadable / corrupt → defaults.
pub fn load_from(path: &Path) -> Appearance {
    match fs::read_to_string(path) {
        Ok(text) => {
            let parsed = Appearance::from_json(&text);
            if parsed == Appearance::default() && serde_json::from_str::<Value>(&text).is_err() {
                eprintln!(
                    "codexbar: {} is not valid JSON, using default appearance",
                    path.display()
                );
            }
            parsed
        }
        Err(_) => Appearance::default(),
    }
}

/// Atomic write (temp file + rename).
pub fn save_to(path: &Path, appearance: &Appearance) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(appearance)
        .map_err(|e| format!("could not serialise appearance: {e}"))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, json.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("could not replace {}: {e}", path.display())
    })
}

// ---------------------------------------------------------------------------
// Taskbar theme (for the tray icons in `system` mode)
// ---------------------------------------------------------------------------

/// Parse the output of
/// `reg query HKCU\...\Themes\Personalize /v <value>` (a `REG_DWORD`).
pub fn parse_reg_dword(output: &str, value: &str) -> Option<bool> {
    output
        .lines()
        .find(|line| line.split_whitespace().next() == Some(value))
        .and_then(|line| line.split_whitespace().last())
        .and_then(|raw| u32::from_str_radix(raw.trim_start_matches("0x"), 16).ok())
        .map(|v| v != 0)
}

#[cfg(windows)]
fn personalize_flag(value: &str) -> Option<bool> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            value,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_reg_dword(&String::from_utf8_lossy(&out.stdout), value)
}

#[cfg(not(windows))]
fn personalize_flag(_value: &str) -> Option<bool> {
    None
}

/// Whether the Windows **taskbar** is light (drives the tray icons).
pub fn system_prefers_light() -> Option<bool> {
    personalize_flag("SystemUsesLightTheme")
}

/// Whether **apps** use the light theme (what `prefers-color-scheme` reports).
pub fn apps_prefer_light() -> Option<bool> {
    personalize_flag("AppsUseLightTheme")
}

/// Native window background per resolved theme — the same values as `--bg`
/// in `ui/styles.css`, so a window that is shown before its first paint never
/// flashes the other theme (RECON B5).
pub fn window_background(light: bool) -> tauri::window::Color {
    if light {
        tauri::window::Color(0xf3, 0xf4, 0xf7, 0xff)
    } else {
        tauri::window::Color(0x0d, 0x0f, 0x13, 0xff)
    }
}

/// Resolve the webview theme for `appearance` (`resolved` is what the page's
/// own `prefers-color-scheme` said, when it told us).
pub fn resolve_light(appearance: &Appearance, resolved: Option<&str>) -> bool {
    match appearance.theme {
        ThemeMode::Light => true,
        ThemeMode::Dark => false,
        ThemeMode::System => match resolved {
            Some("light") => true,
            Some("dark") => false,
            _ => apps_prefer_light().unwrap_or(false),
        },
    }
}

// ---------------------------------------------------------------------------
// Managed state + commands
// ---------------------------------------------------------------------------

/// Managed by Tauri (`app.manage(appearance::AppearanceState::load())`).
pub struct AppearanceState {
    path: PathBuf,
    current: Mutex<Appearance>,
}

impl AppearanceState {
    /// Load from `%APPDATA%` and install the matching tray palette so the very
    /// first tray icons are already drawn in the user's colours.
    pub fn load() -> Self {
        let state = Self::load_at(appearance_path());
        tray_icon::set_palette(state.get().tray_palette(system_prefers_light()));
        state
    }

    /// Load from an explicit file without touching the global tray palette.
    pub fn load_at(path: PathBuf) -> Self {
        let appearance = load_from(&path);
        Self {
            path,
            current: Mutex::new(appearance),
        }
    }

    pub fn get(&self) -> Appearance {
        self.current
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|p| p.into_inner().clone())
    }

    /// Merge, validate and persist `patch`. Returns the stored appearance and
    /// whether anything changed.
    pub fn update(&self, patch: &Value) -> Result<(Appearance, bool), String> {
        let mut guard = self.current.lock().unwrap_or_else(|p| p.into_inner());
        let next = guard.merged_with(patch);
        if next == *guard {
            return Ok((next, false));
        }
        save_to(&self.path, &next)?;
        *guard = next.clone();
        Ok((next, true))
    }
}

/// `invoke("get_appearance", { resolvedTheme? })` → [`Appearance`].
///
/// `theme.js` calls this once per page load and passes the theme it resolved
/// (`light` / `dark`); the calling window's native background is aligned with
/// it so a later show / resize never flashes the other palette.
#[tauri::command]
pub fn get_appearance(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppearanceState>,
    resolved_theme: Option<String>,
) -> Appearance {
    let appearance = state.get();
    let light = resolve_light(&appearance, resolved_theme.as_deref());
    let _ = window.set_background_color(Some(window_background(light)));
    appearance
}

/// `invoke("set_appearance", { appearance })` — a full or partial object.
///
/// Validates and clamps, writes `appearance.json` atomically, repaints the
/// tray, re-tints every window background and emits `appearance-changed`
/// (payload: the stored [`Appearance`]) to every window.
#[tauri::command]
pub fn set_appearance(
    app: AppHandle,
    state: tauri::State<'_, AppearanceState>,
    appearance: Value,
) -> Result<Appearance, String> {
    use tauri::Manager;
    let (next, changed) = state.update(&appearance)?;
    if changed {
        tray_icon::set_palette(next.tray_palette(system_prefers_light()));
        crate::sync_trays(&app);
        let bg = window_background(resolve_light(&next, None));
        for window in app.webview_windows().values() {
            let _ = window.set_background_color(Some(bg));
        }
        let _ = app.emit(APPEARANCE_EVENT, &next);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_file(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("codexbar-appearance-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(APPEARANCE_FILE)
    }

    #[test]
    fn defaults_are_the_historical_dark_look() {
        let a = Appearance::default();
        assert_eq!(a.theme, ThemeMode::System);
        assert_eq!(a.accent, "#6ea8fe");
        assert_eq!(a.font_scale, 100);
        assert_eq!(a.density, Density::Normal);
        assert_eq!(a.popover_opacity, 1.0);
        assert_eq!(a.widget_opacity, 1.0);
        assert!(a.show_legend);
        assert_eq!(a.version, APPEARANCE_VERSION);
    }

    #[test]
    fn serialises_camel_case_contract() {
        let text = serde_json::to_string(&Appearance::default()).unwrap();
        for key in [
            "\"theme\":\"system\"",
            "\"accent\"",
            "\"fontScale\"",
            "\"density\":\"normal\"",
            "\"popoverOpacity\"",
            "\"widgetOpacity\"",
            "\"showLegend\"",
        ] {
            assert!(text.contains(key), "{key} missing in {text}");
        }
    }

    #[test]
    fn opacity_is_clamped_between_0_6_and_1_0() {
        let a = Appearance::from_json(r#"{"popoverOpacity":0.1,"widgetOpacity":7.5}"#);
        assert_eq!(a.popover_opacity, 0.6);
        assert_eq!(a.widget_opacity, 0.6, "7.5 is read as 7.5% then clamped");
        let b = Appearance::from_json(r#"{"popoverOpacity":2,"widgetOpacity":1.4}"#);
        assert_eq!(b.popover_opacity, 0.6, "2 → 2% → clamped to the floor");
        assert_eq!(b.widget_opacity, 0.6);
        let c = Appearance::from_json(r#"{"popoverOpacity":0.85,"widgetOpacity":90}"#);
        assert_eq!(c.popover_opacity, 0.85);
        assert_eq!(c.widget_opacity, 0.9, "percent input accepted");
        let d = Appearance::default().merged_with(&json!({"popoverOpacity": 1.0}));
        assert_eq!(d.popover_opacity, 1.0);
    }

    #[test]
    fn font_scale_is_clamped_between_90_and_130() {
        assert_eq!(Appearance::from_json(r#"{"fontScale":50}"#).font_scale, 90);
        assert_eq!(
            Appearance::from_json(r#"{"fontScale":400}"#).font_scale,
            130
        );
        assert_eq!(
            Appearance::from_json(r#"{"fontScale":115}"#).font_scale,
            115
        );
        assert_eq!(
            Appearance::from_json(r#"{"fontScale":1.2}"#).font_scale,
            120
        );
        let raw = Appearance {
            font_scale: 999,
            ..Appearance::default()
        };
        assert_eq!(raw.normalized().font_scale, 130);
    }

    #[test]
    fn corrupt_json_falls_back_to_defaults() {
        for text in ["", "{", "not json", "[1,2]", "null", "42", "\"dark\""] {
            assert_eq!(
                Appearance::from_json(text),
                Appearance::default(),
                "{text:?}"
            );
        }
        let path = temp_file("corrupt");
        fs::write(&path, b"{\"theme\": \"dark\", ").unwrap();
        assert_eq!(load_from(&path), Appearance::default());
    }

    #[test]
    fn missing_file_gives_defaults() {
        let path = temp_file("missing");
        assert!(!path.exists());
        assert_eq!(load_from(&path), Appearance::default());
    }

    #[test]
    fn a_bad_field_only_resets_that_field() {
        let a = Appearance::from_json(
            r##"{"theme":"neon","accent":"#GGG","fontScale":"big","density":"light","showLegend":false}"##,
        );
        assert_eq!(a.theme, ThemeMode::System);
        assert_eq!(a.accent, DEFAULT_ACCENT);
        assert_eq!(a.font_scale, 100);
        assert_eq!(a.density, Density::Normal);
        assert!(!a.show_legend, "the valid field survives");
    }

    #[test]
    fn accent_is_normalised() {
        assert_eq!(normalize_hex("#ABC").as_deref(), Some("#aabbcc"));
        assert_eq!(normalize_hex("FF8800").as_deref(), Some("#ff8800"));
        assert_eq!(normalize_hex("#12345"), None);
        assert_eq!(normalize_hex("#zzzzzz"), None);
        assert_eq!(hex_to_rgb("#ff8000"), Some([255, 128, 0]));
        let a = Appearance::from_json(r##"{"accent":"#F0A"}"##);
        assert_eq!(a.accent, "#ff00aa");
    }

    #[test]
    fn round_trip_through_disk() {
        let path = temp_file("roundtrip");
        let a = Appearance {
            theme: ThemeMode::Light,
            accent: "#ff5f1f".into(),
            font_scale: 120,
            density: Density::Compact,
            popover_opacity: 0.8,
            widget_opacity: 0.65,
            show_legend: false,
            ..Appearance::default()
        };
        save_to(&path, &a).unwrap();
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file renamed away"
        );
        assert_eq!(load_from(&path), a);
        // Overwrite (rename over an existing file must work on Windows).
        let b = Appearance {
            theme: ThemeMode::Dark,
            ..a.clone()
        };
        save_to(&path, &b).unwrap();
        assert_eq!(load_from(&path), b);
    }

    #[test]
    fn state_update_persists_and_reports_changes() {
        let path = temp_file("state");
        let state = AppearanceState::load_at(path.clone());
        assert_eq!(state.get(), Appearance::default());
        let (next, changed) = state
            .update(&json!({"theme": "dark", "accent": "#22c55e"}))
            .unwrap();
        assert!(changed);
        assert_eq!(next.theme, ThemeMode::Dark);
        assert_eq!(load_from(&path), next);
        let (_, again) = state.update(&json!({"theme": "dark"})).unwrap();
        assert!(!again, "no-op patch must not rewrite");
    }

    #[test]
    fn tray_palette_follows_theme_and_accent() {
        let dark = Appearance::default();
        assert!(!dark.tray_is_light(None));
        assert!(dark.tray_is_light(Some(true)), "system + light taskbar");
        let light = Appearance {
            theme: ThemeMode::Light,
            accent: "#ff0000".into(),
            ..Appearance::default()
        };
        let p = light.tray_palette(Some(false));
        assert_eq!(p.hub, TrayPalette::LIGHT.hub);
        assert_eq!(p.center, [255, 0, 0, 255]);
        let forced_dark = Appearance {
            theme: ThemeMode::Dark,
            ..Appearance::default()
        };
        assert_eq!(
            forced_dark.tray_palette(Some(true)).hub,
            TrayPalette::DARK.hub
        );
    }

    #[test]
    fn parses_reg_query_output() {
        let on = "\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\r\n    SystemUsesLightTheme    REG_DWORD    0x1\r\n";
        let off = on.replace("0x1", "0x0");
        assert_eq!(parse_reg_dword(on, "SystemUsesLightTheme"), Some(true));
        assert_eq!(parse_reg_dword(&off, "SystemUsesLightTheme"), Some(false));
        assert_eq!(parse_reg_dword(on, "AppsUseLightTheme"), None);
        assert_eq!(parse_reg_dword("ERROR", "SystemUsesLightTheme"), None);
    }

    #[test]
    fn resolves_window_theme() {
        let sys = Appearance::default();
        assert!(resolve_light(&sys, Some("light")));
        assert!(!resolve_light(&sys, Some("dark")));
        let dark = Appearance {
            theme: ThemeMode::Dark,
            ..Appearance::default()
        };
        assert!(!resolve_light(&dark, Some("light")), "explicit theme wins");
        assert_eq!(
            window_background(false),
            tauri::window::Color(13, 15, 19, 255)
        );
    }
}
