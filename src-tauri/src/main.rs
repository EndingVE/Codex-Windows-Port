//! CodexBar for Windows — tray app entry point.
//!
//! Lifecycle:
//! 1. `main` handles the offline CLI modes (`--dump-icons`, `--print-settings`,
//!    `--dump-report`, `--config-path`) so evidence can be produced without a
//!    desktop session.
//! 2. `setup` loads [`settings::Settings`] from `%APPDATA%\CodexBar\config.json`,
//!    fetches the first snapshot, installs **one tray icon per enabled provider**
//!    (or a single merged icon), wires the popover window (created hidden by
//!    `tauri.conf.json`) and reconciles the autostart Run entry.
//! 3. A background refresh worker fetches every `refreshIntervalSecs` (default
//!    300 s) **off the UI thread**, providers in parallel with a concurrency cap
//!    and a per-provider deadline, then hands the result to the main thread,
//!    which repaints only the trays that changed and pushes the report to the UI
//!    through the `usage-updated` event (plus `refresh-status`).
//! 4. Left-clicking a tray icon toggles the popover; the tray menu offers Open,
//!    one row per enabled provider, the tray-display submenu (merge / percent /
//!    icon selector), Refresh, Settings and Quit.
//!
//! **Data mode.** The app serves the **live** registry by default — the same
//! `codexbar_providers::live_registry()` the CLI's `--live` uses, so the two can
//! never disagree. It reads this machine's local credentials and each provider
//! reports its own honest status (`ok` / `notConfigured` with the provider's own
//! setup hint / `error` / a token-expired error). Deterministic sample data is
//! served **only** when the user asks for it (`mockMode: true`), and then no
//! credential is read and no socket is opened. See `registry.rs`.
//!
//! Commands exposed to the frontend (the popover UI consumes the first block):
//! `get_report`, `refresh_now`, `get_settings`, `set_settings`, `open_settings`,
//! `quit` — plus `usage_snapshot`/`app_metadata`/`hide_popover`/`quit_app`, kept
//! as aliases so the existing popover keeps working, the settings-window helpers
//! `settings_info`, `default_settings`, `hide_settings`, and the sign-in seam
//! `provider_login` / `copilot_login` (see `login.rs`).

// Keep a console attached in debug builds (useful for `--version`, logs and CI
// evidence); hide it in release so the app behaves like a real tray utility.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appearance;
mod autostart;
mod login;
mod png;
mod reauth;
mod registry;
mod secret_store;
mod settings;
mod settings_window;
mod token_store;
mod tray_icon;
mod widget;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use codexbar_core::refresh::{
    collect_with, retain_last_good, CollectOptions, OutcomeKind, ProviderBackoff, RefreshStatus,
};
use codexbar_core::{DataSource, FetchStatus, ProviderId, ProviderSnapshot, UsageReport};
use codexbar_providers::providers::copilot::{DeviceCode, DeviceFlow};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow,
};

use registry::{CredentialRefreshOutcome, RegistryMode};
use settings::Settings;

/// Tray icon id used by merged mode (and by older builds).
const TRAY_ID: &str = "codexbar-tray";
/// Prefix for the per-provider tray ids: `codexbar-tray-<provider>`.
const TRAY_PREFIX: &str = "codexbar-tray-";
/// Popover window label (must match `tauri.conf.json`).
const POPOVER: &str = "popover";
/// Tray size in pixels. Windows scales this down for the 16 px tray slot.
const TRAY_PX: u32 = 32;

/// How long to wait for the popover's page-load event before showing it anyway.
const POPOVER_FALLBACK_MS: u64 = 2500;

/// True once the popover page has reported `PageLoadEvent::Finished`.
///
/// The popover is created hidden from `tauri.conf.json`, so showing it before
/// WebView2 has composed a frame paints an empty rectangle. This (plus the dark
/// `backgroundColor` in the window config) is what keeps that first frame dark.
static POPOVER_READY: AtomicBool = AtomicBool::new(false);
/// Set when the popover was asked for while it was still loading.
static POPOVER_PENDING_SHOW: AtomicBool = AtomicBool::new(false);
/// Guards against piling up popover fallback timers.
static POPOVER_FALLBACK_ARMED: AtomicBool = AtomicBool::new(false);

/// Shared application state.
struct AppState {
    /// Latest published report. `Arc` so readers (commands, tray repaint) do
    /// not deep-copy it on every access.
    report: Mutex<Arc<UsageReport>>,
    settings: Mutex<Settings>,
    /// Ids of the tray icons currently installed, in display order.
    tray_ids: Mutex<Vec<String>>,
    /// What each installed tray currently shows (icon key, tooltip) plus the
    /// menu signature, so a refresh only touches what changed.
    tray_cache: Mutex<TrayCache>,
    /// Per-provider summary of the last refresh cycle (errors, stale, backoff).
    refresh_status: Mutex<Option<RefreshStatus>>,
    /// Provider registry reused across ticks (rebuilt on mode/config change).
    registry: registry::RegistryCache,
    /// Channel to the refresh worker (see [`spawn_refresh_worker`]).
    refresh_tx: Mutex<Option<mpsc::Sender<RefreshRequest>>>,
    /// A started Copilot device flow, if one is waiting for the user.
    ///
    /// The device code stays in this process: it is the handshake secret for
    /// `copilot_login_poll` and never enters a payload the webview can read.
    copilot_flow: Mutex<Option<DeviceCode>>,
    /// The re-authentication state machine (launching `codex login` / `claude`).
    reauth: Mutex<reauth::ReauthManager>,
}

impl AppState {
    fn new(settings: Settings) -> Self {
        Self {
            report: Mutex::new(Arc::new(UsageReport::new(Vec::new()))),
            settings: Mutex::new(settings),
            tray_ids: Mutex::new(Vec::new()),
            tray_cache: Mutex::new(TrayCache::default()),
            refresh_status: Mutex::new(None),
            registry: registry::RegistryCache::default(),
            refresh_tx: Mutex::new(None),
            copilot_flow: Mutex::new(None),
            reauth: Mutex::new(reauth::ReauthManager::new()),
        }
    }

    fn snapshot(&self) -> Arc<UsageReport> {
        Arc::clone(
            &self
                .report
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    fn store(&self, report: Arc<UsageReport>) {
        let mut guard = self
            .report
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = report;
    }

    fn refresh_status(&self) -> Option<RefreshStatus> {
        self.refresh_status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn set_refresh_status(&self, status: RefreshStatus) {
        *self
            .refresh_status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(status);
    }

    /// Queue a request for the refresh worker. Never blocks.
    fn request_refresh(&self, request: RefreshRequest) -> bool {
        self.refresh_tx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .is_some_and(|tx| tx.send(request).is_ok())
    }

    fn settings(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn set_settings(&self, settings: Settings) {
        let mut guard = self
            .settings
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = settings;
    }

    fn tray_ids(&self) -> Vec<String> {
        self.tray_ids
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn set_tray_ids(&self, ids: Vec<String>) {
        let mut guard = self
            .tray_ids
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = ids;
    }

    /// The pending device flow, if one is waiting for the user.
    ///
    /// Cloned for a poll (the device code is an in-process handshake value; it
    /// never enters a payload the webview can read).
    fn copilot_flow(&self) -> Option<DeviceCode> {
        self.copilot_flow
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn set_copilot_flow(&self, flow: Option<DeviceCode>) {
        let mut guard = self
            .copilot_flow
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = flow;
    }
}

// ---------------------------------------------------------------------------
// Commands exposed to the frontend
// ---------------------------------------------------------------------------

/// Current report. The UI calls this on load, then waits for `usage-updated`.
#[tauri::command]
fn get_report(state: tauri::State<'_, AppState>) -> UsageReport {
    (*state.snapshot()).clone()
}

/// Alias kept for the popover as written in Ola 1A/1B.
#[tauri::command]
fn usage_snapshot(state: tauri::State<'_, AppState>) -> UsageReport {
    (*state.snapshot()).clone()
}

/// Force a provider refresh and return the fresh report.
///
/// `async` so the webview's IPC never runs the fetch on the main thread: the
/// request is queued to the refresh worker and awaited on a blocking pool
/// thread. Backoff is bypassed for a manual refresh.
#[tauri::command]
async fn refresh_now(app: AppHandle) -> UsageReport {
    let (reply_tx, reply_rx) = mpsc::channel();
    let state = app.state::<AppState>();
    if !state.request_refresh(RefreshRequest::Now {
        reply: Some(reply_tx),
    }) {
        return (*state.snapshot()).clone();
    }
    let waited = tauri::async_runtime::spawn_blocking(move || reply_rx.recv()).await;
    match waited {
        Ok(Ok(report)) => (*report).clone(),
        _ => (*app.state::<AppState>().snapshot()).clone(),
    }
}

/// Per-provider outcome of the last refresh cycle: failure class (`auth`,
/// `rateLimited`, `network`, `timeout`, `server`, `other`), whether the card
/// shows last-known (stale) values, backoff countdown, and the global
/// `offline` flag. `null` until the first cycle finished.
#[tauri::command]
fn refresh_status(state: tauri::State<'_, AppState>) -> Option<RefreshStatus> {
    state.refresh_status()
}

/// Current settings (camelCase JSON, as persisted).
#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings().for_webview()
}

/// Factory defaults, for the settings window's "Restore defaults" button.
#[tauri::command]
fn default_settings() -> Settings {
    Settings::default()
}

/// Persist settings and apply them (tray layout, autostart, refresh cadence).
#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    apply_settings(&app, settings)
}

/// Paths and host facts the settings window displays.
#[tauri::command]
fn settings_info(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let path = settings::config_path();
    let autostart_command = autostart::registered_command();
    let settings = state.settings();
    serde_json::json!({
        "configPath": path.to_string_lossy(),
        "configDir": settings::config_dir().to_string_lossy(),
        "exePath": autostart::exe_path().unwrap_or_else(|_| String::new()),
        "autostartKey": autostart::RUN_KEY,
        "autostartValueName": autostart::VALUE_NAME,
        "autostartEnabled": autostart_command.is_some(),
        "autostartCommand": autostart_command.unwrap_or_default(),
        "version": env!("CARGO_PKG_VERSION"),
        "mode": RegistryMode::from_settings(&settings).as_str(),
        "mockMode": settings.mock_mode,
        "refreshCredentials": settings.refresh_credentials,
        // False until some provider exposes a startable sign-in flow, so a
        // settings page can hide the button instead of offering a dead one.
        "loginFlowsAvailable": login::any_flow_available(),
        // AuthKind + the sign-in seam per provider, so a settings page can pick
        // "paste your key" vs "sign in with the provider's CLI" vs the Copilot
        // device flow without asking a provider that may be unreachable.
        // Providers whose session is owned by a CLI this port can launch to
        // re-authenticate (`codex login`, `claude`). The port never refreshes a
        // token itself — it delegates to the owning CLI, exactly like the
        // original app.
        "reauthCatalog": ProviderId::ALL
            .iter()
            .filter_map(|id| {
                reauth::command_for(*id).map(|command| {
                    serde_json::json!({
                        "id": id.as_str(),
                        "title": id.title(),
                        "command": command.label,
                    })
                })
            })
            .collect::<Vec<_>>(),
        "catalog": login::catalog()
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "id": entry.provider,
                    "title": entry.title,
                    "authKind": entry.auth_kind,
                    "loginAvailable": entry.available,
                    "loginStatus": entry.status,
                    "loginInstructions": entry.instructions,
                    "loginExtensionPoint": entry.extension_point,
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// Show the settings window (creating it on first call).
#[tauri::command]
fn open_settings(app: AppHandle) {
    settings_window::open(&app);
}

/// Hide the settings window.
#[tauri::command]
fn hide_settings(app: AppHandle) {
    settings_window::hide(&app);
}

/// Hide the popover (the UI's ✕ button and the Esc key).
#[tauri::command]
fn hide_popover(app: AppHandle) {
    if let Some(win) = app.get_webview_window(POPOVER) {
        let _ = win.hide();
    }
}

/// Quit the whole app from the UI.
#[tauri::command]
fn quit(app: AppHandle) {
    app.exit(0);
}

/// Alias kept for the popover as written in Ola 1A/1B.
#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

/// Build/version facts the UI shows in the footer.
#[tauri::command]
fn app_metadata(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let settings = state.settings();
    let mode = RegistryMode::from_settings(&settings);
    serde_json::json!({
        "name": "CodexBar for Windows",
        "version": env!("CARGO_PKG_VERSION"),
        "schemaVersion": codexbar_core::SCHEMA_VERSION,
        "providerCount": ProviderId::ALL.len(),
        "enabledProviders": settings.enabled_providers().len(),
        "mergeIcons": settings.merge_icons,
        "refreshIntervalSecs": settings.refresh_interval_secs,
        // Both the active mode and the raw setting: the UI can show "live" and
        // still tell the user why it is sample data.
        "mode": mode.as_str(),
        "mockMode": settings.mock_mode,
        "refreshCredentials": settings.refresh_credentials,
        "configPath": settings::config_path().to_string_lossy(),
    })
}

/// Sign-in story for one provider — the device-flow extension point.
///
/// Returns `notAvailable` (with the file that has to grow the flow) rather than
/// starting a fake login: see `login.rs`. `provider` accepts the same tolerant
/// spellings as the settings file (`Copilot`, `copilot`, `OpenCode Go`, …).
#[tauri::command]
fn provider_login(provider: String) -> Result<login::LoginStatus, String> {
    let id = ProviderId::from_str_lossy(&provider)
        .ok_or_else(|| format!("unknown provider: {provider}"))?;
    Ok(login::status(id))
}

/// Copilot's device-flow login, named for the provider it serves.
#[tauri::command]
fn copilot_login() -> login::LoginStatus {
    login::status(ProviderId::Copilot)
}

/// Step 1 of the GitHub device flow: ask for a code the user types in the
/// browser.
///
/// The device code is kept in [`AppState`]; the webview only ever sees the
/// `userCode` and the verification URL.
#[tauri::command]
fn copilot_login_start(state: tauri::State<'_, AppState>) -> Result<login::LoginStarted, String> {
    let started = login::start(&DeviceFlow::new())?;
    // Keep the device code server-side; hand the webview the user code only.
    state.set_copilot_flow(Some(started.device));
    Ok(started.payload)
}

/// Step 2: one poll of the started flow. Terminal outcomes forget the flow.
///
/// A successful authorization is persisted to the port's config by
/// [`token_store`]; the token never reaches the webview.
#[tauri::command]
fn copilot_login_poll(state: tauri::State<'_, AppState>) -> Result<login::LoginPoll, String> {
    let Some(code) = state.copilot_flow() else {
        return Err("No GitHub sign-in is waiting — start it again.".to_string());
    };
    let path = token_store::config_path();
    let result = login::poll(&DeviceFlow::new(), &code, Some(&path));
    if result.status == "authorized" {
        state.set_settings(settings::load());
    }
    if matches!(
        result.status.as_str(),
        "authorized" | "expired" | "denied" | "failed"
    ) {
        state.set_copilot_flow(None);
    }
    Ok(result)
}

/// Forget a started flow the user abandoned (the ✕ button in Settings).
#[tauri::command]
fn copilot_login_cancel(state: tauri::State<'_, AppState>) {
    state.set_copilot_flow(None);
}

// ---------------------------------------------------------------------------
// Re-authentication (delegate to the CLI that owns the session)
// ---------------------------------------------------------------------------

/// Launch the owning CLI's sign-in for `provider` (`codex login`, `claude`).
///
/// The CLI runs in its own visible console and writes its own credentials; this
/// process never captures, parses or stores anything it prints.
#[tauri::command]
fn reauth_launch(
    state: tauri::State<'_, AppState>,
    provider: String,
) -> Result<reauth::ReauthStatus, String> {
    let id = ProviderId::from_str_lossy(&provider)
        .ok_or_else(|| format!("unknown provider: {provider}"))?;
    let mut manager = state
        .reauth
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let status = manager.start(id)?;
    eprintln!(
        "codexbar: re-auth launched for {provider}: {} (pid {:?})",
        status.command, status.pid
    );
    Ok(status)
}

/// Poll the re-auth state machine (`idle` | `launched` | `running` | `finished`).
#[tauri::command]
fn reauth_status(state: tauri::State<'_, AppState>) -> reauth::ReauthStatus {
    let mut manager = state
        .reauth
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    manager.status()
}

/// Stop a running re-auth (the CLI's window is closed; nothing is written).
#[tauri::command]
fn reauth_cancel(state: tauri::State<'_, AppState>) -> reauth::ReauthStatus {
    let mut manager = state
        .reauth
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    manager.cancel()
}

/// Per-provider session state plus whether a CLI re-auth can be launched.
///
/// This is the seam the settings page (and a tokenExpired card) reads: it maps
/// the frozen `FetchStatus` onto the port's honest vocabulary and names the
/// owning CLI's login command for the providers whose session this port cannot
/// refresh itself.
#[tauri::command]
fn session_states(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let report = state.snapshot();
    let sessions = report
        .providers
        .iter()
        .map(|snapshot| {
            let expired = snapshot.status == FetchStatus::Error
                && snapshot.error.as_deref().is_some_and(looks_like_token_expired);
            let command = reauth::command_for(snapshot.provider);
            serde_json::json!({
                "provider": snapshot.provider.as_str(),
                "title": snapshot.title,
                "status": match snapshot.status {
                    FetchStatus::Ok => "ok",
                    FetchStatus::Stale => "stale",
                    FetchStatus::NotConfigured => "notConfigured",
                    FetchStatus::Error => "error",
                },
                "sessionState": if expired {
                    "expired"
                } else if snapshot.status == FetchStatus::Error {
                    "error"
                } else {
                    "usable"
                },
                "tokenExpired": expired,
                "detail": snapshot.error.clone().unwrap_or_default(),
                "canReauth": reauth::can_launch(snapshot.provider),
                "reauthCommand": command.as_ref().map(|c| c.label.clone()),
                "reauthHint": command.as_ref().map(|c| format!(
                    "This port does not refresh tokens. Run \"{}\" to sign in again; the CLI writes its own credential.",
                    c.label
                )),
            })
        })
        .collect::<Vec<_>>();
    let reauth = {
        let mut manager = state
            .reauth
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        manager.status()
    };
    serde_json::json!({ "sessions": sessions, "reauth": reauth })
}

// ---------------------------------------------------------------------------
// Report helpers
// ---------------------------------------------------------------------------

/// Error prose that means "the stored credential is no longer usable".
///
/// Mirrors the classifier in `ui/format.js` (`TOKEN_RE`), because the frozen
/// `FetchStatus` enum has no `tokenExpired` variant: a provider reports
/// `Error` plus a message, and both surfaces have to read the same signal or
/// the card and the tray tooltip would disagree about the same provider.
fn looks_like_token_expired(error: &str) -> bool {
    let text = error.to_ascii_lowercase();
    const NEEDLES: [&str; 8] = [
        "token expired",
        "token is expired",
        "expired token",
        "token revoked",
        "re-authenticate",
        "reauth",
        "sign in again",
        "unauthorized",
    ];
    NEEDLES.iter().any(|needle| text.contains(needle))
        || text.contains("expir") && text.contains("token")
        || text.contains("401")
        || text.contains("invalid_grant")
}

/// Honest one-line state of a provider, for the tray menu and tooltips.
///
/// Never invents a number: a provider with no usable lane says so instead of
/// rendering `0%`, which is the macOS `5h 0%`-from-`null` bug this port keeps
/// out (see CONTRACT.md).
fn describe(provider: &ProviderSnapshot) -> String {
    match provider.status {
        FetchStatus::NotConfigured => "not configured".to_string(),
        FetchStatus::Error => {
            if provider
                .error
                .as_deref()
                .is_some_and(looks_like_token_expired)
            {
                "token expired".to_string()
            } else {
                "fetch failed".to_string()
            }
        }
        FetchStatus::Stale => match provider.headline_used_percent() {
            Some(pct) => format!("{pct:.0}% used (stale)"),
            None => "stale, no numbers".to_string(),
        },
        FetchStatus::Ok => match provider.headline_used_percent() {
            Some(pct) => format!("{pct:.0}% used"),
            None => "no quota window".to_string(),
        },
    }
}

/// Where a provider's numbers come from, in the same vocabulary the popover
/// badge uses (`ui/render.js` renders `source-<source>` per card).
///
/// `sample data` is the mock registry; `live/oauth` and friends mean a real
/// fetch path. A provider that failed still reports its live source, and the
/// state half of the tooltip says there is no data — the two together are the
/// "live vs mock vs no data" signal.
fn source_label(provider: &ProviderSnapshot) -> String {
    if provider.source == DataSource::Mock {
        "sample data".to_string()
    } else {
        format!("live/{}", provider.source.as_str())
    }
}

/// Headline percentage for one provider, or `None` when there is nothing real
/// to draw (not configured, fetch failed, or only synthetic lanes).
fn percent_of(report: &UsageReport, id: ProviderId) -> Option<f64> {
    let snapshot = report.providers.iter().find(|p| p.provider == id)?;
    match snapshot.status {
        // `stale` keeps the last good numbers on purpose (see CONTRACT.md).
        FetchStatus::Ok | FetchStatus::Stale => snapshot.headline_used_percent(),
        _ => None,
    }
}

/// The percentage the merged icon mirrors: the pinned provider, else the worst
/// of the enabled ones.
fn merged_percent(report: &UsageReport, settings: &Settings) -> Option<f64> {
    if let Some(id) = settings
        .merged_provider
        .as_deref()
        .and_then(ProviderId::from_str_lossy)
    {
        return percent_of(report, id);
    }
    settings
        .enabled_providers()
        .into_iter()
        .filter_map(|id| percent_of(report, id))
        .fold(None, |acc: Option<f64>, pct| {
            Some(acc.map_or(pct, |a| a.max(pct)))
        })
}

/// Windows caps tray tooltips around 127 characters, so this trims instead of
/// letting the shell cut mid-word. Only enabled providers are listed — the
/// tooltip must describe the icons the user actually asked for — and the mode
/// tag ("sample data") is what tells a glance apart a mock report from a live
/// one without opening the popover.
fn merged_tooltip(report: &UsageReport, settings: &Settings) -> String {
    let mode = RegistryMode::from_settings(settings);
    let mode_tag = match mode {
        RegistryMode::Mock => " · sample data",
        RegistryMode::Live => "",
    };
    let budget = 122 - mode_tag.len();

    let mut text = String::new();
    for id in settings.enabled_providers() {
        let Some(provider) = report.providers.iter().find(|p| p.provider == id) else {
            continue;
        };
        if provider.status != FetchStatus::Ok {
            continue;
        }
        let Some(pct) = provider.headline_used_percent() else {
            continue;
        };
        let piece = format!("{} {pct:.0}%", provider.title);
        if text.len() + piece.len() + 3 > budget {
            break;
        }
        if !text.is_empty() {
            text.push_str(" · ");
        }
        text.push_str(&piece);
    }
    if text.is_empty() {
        text.push_str("CodexBar — no provider data");
    }
    text.push_str(mode_tag);
    text
}

/// Tooltip for a single provider icon, e.g.
/// `Codex — token expired · live/oauth` or `Gemini — not configured · live/apiKey`.
fn provider_tooltip(report: &UsageReport, id: ProviderId) -> String {
    match report.providers.iter().find(|p| p.provider == id) {
        Some(snapshot) => format!(
            "{} — {} · {}",
            snapshot.title,
            describe(snapshot),
            source_label(snapshot)
        ),
        None => format!("{} — no data · live/pending", id.title()),
    }
}

/// The RGBA image actually handed to the tray for a provider.
fn provider_icon(
    report: &UsageReport,
    id: ProviderId,
    show_percent: bool,
) -> tauri::image::Image<'static> {
    tray_icon::icon_image(TRAY_PX, percent_of(report, id), show_percent)
}

/// The RGBA image handed to the merged tray.
fn merged_icon(report: &UsageReport, settings: &Settings) -> tauri::image::Image<'static> {
    tray_icon::icon_image(
        TRAY_PX,
        merged_percent(report, settings),
        settings.show_percent_in_icon,
    )
}

// ---------------------------------------------------------------------------
// Tray
// ---------------------------------------------------------------------------

/// Ids of the tray icons the current settings call for.
///
/// Merged mode is a single icon. With no provider enabled we still keep one
/// icon, otherwise the user would have no way to reach the menu and quit.
fn desired_tray_ids(settings: &Settings) -> Vec<String> {
    let enabled = settings.enabled_providers();
    if settings.merge_icons || enabled.is_empty() {
        return vec![TRAY_ID.to_string()];
    }
    enabled
        .into_iter()
        .map(|id| format!("{TRAY_PREFIX}{}", id.as_str()))
        .collect()
}

/// Build the tray menu for the current report + settings.
///
/// In merged mode the "Tray icons ▸ Icon follows ▸" submenu is the selector the
/// merged icon mirrors; per-provider mode hides it because the icons are the
/// selector.
fn build_menu(
    app: &AppHandle,
    report: &UsageReport,
    settings: &Settings,
) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    let enabled = settings.enabled_providers();

    menu.append(&MenuItem::with_id(
        app,
        "open",
        "Open CodexBar",
        true,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    for id in &enabled {
        let label = match report.providers.iter().find(|p| p.provider == *id) {
            Some(snapshot) => format!(
                "{}   {} · {}",
                snapshot.title,
                describe(snapshot),
                source_label(snapshot)
            ),
            None => format!("{}   no data · live/pending", id.title()),
        };
        // Disabled row: pure read-out, clicking does nothing.
        menu.append(&MenuItem::with_id(
            app,
            format!("row:{}", id.as_str()),
            label,
            false,
            None::<&str>,
        )?)?;
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;

    let display = Submenu::with_id(app, "tray", "Tray icons", true)?;
    display.append(&CheckMenuItem::with_id(
        app,
        "tray:merge",
        "Merge into one icon",
        true,
        settings.merge_icons,
        None::<&str>,
    )?)?;
    display.append(&CheckMenuItem::with_id(
        app,
        "tray:percent",
        "Show percentage in icon",
        true,
        settings.show_percent_in_icon,
        None::<&str>,
    )?)?;

    if settings.merge_icons {
        display.append(&PredefinedMenuItem::separator(app)?)?;
        let selector = Submenu::with_id(app, "sel", "Icon follows", true)?;
        selector.append(&CheckMenuItem::with_id(
            app,
            "sel:worst",
            "Worst provider",
            true,
            settings.merged_provider.is_none(),
            None::<&str>,
        )?)?;
        for id in &enabled {
            selector.append(&CheckMenuItem::with_id(
                app,
                format!("sel:{}", id.as_str()),
                id.title(),
                true,
                settings.merged_provider.as_deref() == Some(id.as_str()),
                None::<&str>,
            )?)?;
        }
        display.append(&selector)?;
    }
    menu.append(&display)?;
    menu.append(&widget::submenu(app)?)?;

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        "refresh",
        "Refresh now",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "settings",
        "Settings…",
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        "quit",
        "Quit CodexBar",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

/// Handle a tray menu selection from any of the installed icons.
fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        "open" => toggle_popover(app),
        "refresh" => {
            // Queued to the worker: the menu handler runs on the main thread
            // and must never wait for the network.
            app.state::<AppState>()
                .request_refresh(RefreshRequest::Now { reply: None });
        }
        "settings" => settings_window::open(app),
        "quit" => app.exit(0),
        other if other.starts_with(widget::MENU_PREFIX) => {
            widget::handle_menu(app, other);
            sync_trays(app);
        }
        "tray:merge" => mutate_settings(app, |s| s.merge_icons = !s.merge_icons),
        "tray:percent" => {
            mutate_settings(app, |s| s.show_percent_in_icon = !s.show_percent_in_icon)
        }
        other => {
            if let Some(rest) = other.strip_prefix("sel:") {
                let selection = if rest == "worst" {
                    None
                } else {
                    Some(rest.to_string())
                };
                mutate_settings(app, move |s| s.merged_provider = selection.clone());
            }
        }
    }
}

/// What the installed trays currently display, so a refresh can skip the
/// shell calls (and the menu rebuild) when nothing visible changed.
#[derive(Debug, Default, Clone, PartialEq)]
struct TrayCache {
    /// tray id -> (icon key, tooltip)
    visuals: HashMap<String, (String, String)>,
    /// Signature of the menu currently attached to every tray.
    menu: Option<String>,
}

/// What `sync_trays` has to do for a given installed / wanted icon set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrayPlan {
    /// Same icons: update the changed ones in place (no remove / re-create).
    UpdateInPlace,
    /// The set changed (provider toggled, merge flipped): rebuild the trays.
    Reinstall,
}

fn tray_plan(current: &[String], desired: &[String]) -> TrayPlan {
    if current == desired {
        TrayPlan::UpdateInPlace
    } else {
        TrayPlan::Reinstall
    }
}

/// Cheap identity of a tray icon's pixels: the percent it draws (to 0.1) and
/// whether the number is printed.
fn icon_key(percent: Option<f64>, show_percent: bool) -> String {
    match percent {
        Some(p) => format!("{:.1}|{show_percent}", p),
        None => format!("none|{show_percent}"),
    }
}

/// Icon key + tooltip a given tray id should show.
fn tray_visual(report: &UsageReport, settings: &Settings, tray_id: &str) -> (String, String) {
    if tray_id == TRAY_ID {
        (
            icon_key(
                merged_percent(report, settings),
                settings.show_percent_in_icon,
            ),
            merged_tooltip(report, settings),
        )
    } else if let Some(provider) = tray_id
        .strip_prefix(TRAY_PREFIX)
        .and_then(ProviderId::from_str_lossy)
    {
        (
            icon_key(percent_of(report, provider), settings.show_percent_in_icon),
            provider_tooltip(report, provider),
        )
    } else {
        (String::new(), String::new())
    }
}

/// Everything `build_menu` renders, as one string: equal signatures mean an
/// identical menu, so it does not have to be rebuilt.
fn menu_signature(report: &UsageReport, settings: &Settings) -> String {
    let mut sig = format!(
        "{}|{}|{:?}|",
        settings.merge_icons, settings.show_percent_in_icon, settings.merged_provider
    );
    for id in settings.enabled_providers() {
        match report.providers.iter().find(|p| p.provider == id) {
            Some(snapshot) => sig.push_str(&format!(
                "{}:{}:{};",
                id.as_str(),
                describe(snapshot),
                source_label(snapshot)
            )),
            None => sig.push_str(&format!("{}:-;", id.as_str())),
        }
    }
    sig
}

fn tray_cache<'a>(state: &'a AppState) -> std::sync::MutexGuard<'a, TrayCache> {
    state
        .tray_cache
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Remove every installed tray icon.
fn remove_trays(app: &AppHandle) {
    let ids = app.state::<AppState>().tray_ids();
    for id in ids {
        app.remove_tray_by_id(&id);
    }
    app.state::<AppState>().set_tray_ids(Vec::new());
}

/// (Re)create the tray icons the current settings ask for.
fn install_trays(app: &AppHandle) -> tauri::Result<()> {
    remove_trays(app);

    let state = app.state::<AppState>();
    let report = state.snapshot();
    let settings = state.settings();
    let enabled = settings.enabled_providers();
    let mut installed: Vec<String> = Vec::new();

    let per_provider = !settings.merge_icons && !enabled.is_empty();

    if per_provider {
        for id in enabled {
            let tray_id = format!("{TRAY_PREFIX}{}", id.as_str());
            let menu = build_menu(app, &report, &settings)?;
            TrayIconBuilder::with_id(tray_id.clone())
                .icon(provider_icon(&report, id, settings.show_percent_in_icon))
                .tooltip(provider_tooltip(&report, id))
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_popover(tray.app_handle());
                    }
                })
                .build(app)?;
            installed.push(tray_id);
        }
    } else {
        let menu = build_menu(app, &report, &settings)?;
        TrayIconBuilder::with_id(TRAY_ID)
            .icon(merged_icon(&report, &settings))
            .tooltip(merged_tooltip(&report, &settings))
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = event
                {
                    toggle_popover(tray.app_handle());
                }
            })
            .build(app)?;
        installed.push(TRAY_ID.to_string());
    }

    let mut cache = TrayCache {
        visuals: HashMap::new(),
        menu: Some(menu_signature(&report, &settings)),
    };
    for id in &installed {
        cache
            .visuals
            .insert(id.clone(), tray_visual(&report, &settings, id));
    }
    *tray_cache(&state) = cache;
    app.state::<AppState>().set_tray_ids(installed);
    Ok(())
}

/// Repaint the installed trays with the latest report. **Main thread only.**
///
/// When the icon set changed (a provider was enabled/disabled, merge toggled)
/// the trays are rebuilt. Otherwise nothing is removed or re-created: each
/// icon's image and tooltip are replaced only when they differ from what it
/// shows, and the menu is rebuilt only when its content changed.
fn sync_trays(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings();
    let desired = desired_tray_ids(&settings);
    let current = state.tray_ids();

    if tray_plan(&current, &desired) == TrayPlan::Reinstall {
        if let Err(err) = install_trays(app) {
            eprintln!("codexbar: could not install tray icons: {err}");
        }
        return;
    }

    let report = state.snapshot();
    let signature = menu_signature(&report, &settings);
    let mut cache = tray_cache(&state);
    let menu = if cache.menu.as_deref() == Some(signature.as_str()) {
        None
    } else {
        match build_menu(app, &report, &settings) {
            Ok(menu) => {
                cache.menu = Some(signature);
                Some(menu)
            }
            Err(err) => {
                eprintln!("codexbar: could not rebuild the tray menu: {err}");
                None
            }
        }
    };

    for id in &current {
        let Some(tray) = app.tray_by_id(id) else {
            continue;
        };
        let (key, tooltip) = tray_visual(&report, &settings, id);
        let shown = cache.visuals.get(id).cloned().unwrap_or_default();
        if shown.0 != key {
            let image = if id == TRAY_ID {
                merged_icon(&report, &settings)
            } else {
                let provider = id
                    .strip_prefix(TRAY_PREFIX)
                    .and_then(ProviderId::from_str_lossy)
                    .unwrap_or(ProviderId::Codex);
                provider_icon(&report, provider, settings.show_percent_in_icon)
            };
            let _ = tray.set_icon(Some(image));
        }
        if shown.1 != tooltip {
            let _ = tray.set_tooltip(Some(tooltip.clone()));
        }
        cache.visuals.insert(id.clone(), (key, tooltip));
        if let Some(menu) = &menu {
            let _ = tray.set_menu(Some(menu.clone()));
        }
    }
}

/// Persist + apply a settings change made from the tray menu.
fn mutate_settings(app: &AppHandle, mutate: impl FnOnce(&mut Settings)) {
    let state = app.state::<AppState>();
    let mut next = state.settings();
    mutate(&mut next);
    let next = next.normalized();
    if let Err(err) = settings::save_merged(&next) {
        eprintln!("codexbar: could not save settings: {err}");
    }
    state.set_settings(next.clone());
    state.request_refresh(RefreshRequest::SettingsChanged);
    // Autostart is not touched here: no tray item changes it.
    sync_trays(app);
    let _ = app.emit_to(POPOVER, "settings-updated", &next);
}

/// Full apply path for the `set_settings` command.
///
/// Idempotent: when the incoming settings match the ones already in force there
/// is nothing to persist and nothing to rebuild, so the disk write, the tray
/// pass and the stderr line are all skipped. A UI that saves on every
/// interaction (the settings window does, once per toggle) therefore writes
/// once per real change rather than once per click.
fn apply_settings(app: &AppHandle, incoming: Settings) -> Result<Settings, String> {
    let next = incoming.normalized();
    if next == app.state::<AppState>().settings() {
        return Ok(next);
    }
    let path = settings::save_merged(&next)?;
    app.state::<AppState>().set_settings(next.clone());
    app.state::<AppState>()
        .request_refresh(RefreshRequest::SettingsChanged);

    autostart::reconcile(next.start_at_login);
    sync_trays(app);
    let _ = app.emit_to(POPOVER, "settings-updated", &next);
    let _ = app.emit_to(settings_window::SETTINGS_WINDOW, "settings-updated", &next);
    eprintln!("codexbar: settings saved to {}", path.display());
    Ok(next)
}

// ---------------------------------------------------------------------------
// Popover window
// ---------------------------------------------------------------------------

/// Park the popover above the notification area, the way Windows tray flyouts do.
fn position_popover(win: &WebviewWindow) {
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten());
    let Some(monitor) = monitor else {
        return;
    };
    let Ok(win_size) = win.outer_size() else {
        return;
    };

    let scale = monitor.scale_factor();
    let origin = monitor.position();
    let screen = monitor.size();
    let right = origin.x as f64 + screen.width as f64;
    let bottom = origin.y as f64 + screen.height as f64;
    // 12 px from the right edge, ~56 px above the bottom (taskbar height).
    let x = right - win_size.width as f64 - 12.0 * scale;
    let y = bottom - win_size.height as f64 - 56.0 * scale;
    let _ = win.set_position(PhysicalPosition::new(x.max(0.0), y.max(0.0)));
}

/// Show/hide the popover, positioning it next to the tray when it opens.
fn toggle_popover(app: &AppHandle) {
    let Some(win) = app.get_webview_window(POPOVER) else {
        return;
    };
    if win.is_visible().unwrap_or(false) {
        let _ = win.hide();
        return;
    }
    show_popover(app, &win);
}

/// Show the popover, waiting for its first paint when the page is not ready.
///
/// On a cold start (`--show`, or a tray click in the first second) WebView2 has
/// not composed anything yet; showing then paints an empty rectangle. The dark
/// `backgroundColor` from `tauri.conf.json` already covers that frame, and this
/// gate removes it entirely by showing only once the page has loaded.
fn show_popover(app: &AppHandle, win: &WebviewWindow) {
    position_popover(win);
    if POPOVER_READY.load(Ordering::SeqCst) {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    POPOVER_PENDING_SHOW.store(true, Ordering::SeqCst);
    if POPOVER_FALLBACK_ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(POPOVER_FALLBACK_MS));
        POPOVER_FALLBACK_ARMED.store(false, Ordering::SeqCst);
        if !POPOVER_PENDING_SHOW.swap(false, Ordering::SeqCst) {
            return; // the page-load hook already showed it
        }
        if let Some(win) = handle.get_webview_window(POPOVER) {
            eprintln!("codexbar: popover page did not report a load; showing it anyway");
            let _ = win.show();
            let _ = win.set_focus();
        }
    });
}

/// Show the popover once its page can paint.
///
/// Wired as the global page-load hook so the popover (created from
/// `tauri.conf.json`, not from a builder) gets the same treatment as the
/// settings window.
fn on_any_page_load(
    webview: &tauri::Webview<tauri::Wry>,
    payload: &tauri::webview::PageLoadPayload<'_>,
) {
    if !matches!(payload.event(), tauri::webview::PageLoadEvent::Finished) {
        return;
    }
    if webview.label() != POPOVER {
        return;
    }
    POPOVER_READY.store(true, Ordering::SeqCst);
    if POPOVER_PENDING_SHOW.swap(false, Ordering::SeqCst) {
        let win = webview.window();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

// ---------------------------------------------------------------------------
// Refresh pipeline
// ---------------------------------------------------------------------------

/// True when `elapsed` has reached the configured cadence.
///
/// This is the gate that makes `refreshIntervalSecs` authoritative (and keeps a
/// lower bound of 1 s even for a hand-edited config).
fn refresh_due(elapsed: Duration, interval_secs: u64) -> bool {
    elapsed >= Duration::from_secs(interval_secs.max(1))
}

/// How long the worker may sleep before the next scheduled refresh.
fn time_until_due(elapsed: Duration, interval_secs: u64) -> Duration {
    Duration::from_secs(interval_secs.max(1)).saturating_sub(elapsed)
}

/// Messages for the refresh worker.
enum RefreshRequest {
    /// Refresh right away, ignoring per-provider backoff (tray "Refresh now",
    /// `refresh_now`, first fetch). `reply` receives the published report.
    Now {
        reply: Option<mpsc::Sender<Arc<UsageReport>>>,
    },
    /// Settings were saved: re-read the interval, rebuild the registry, and
    /// refresh at once if the data mode (live / mock) changed.
    SettingsChanged,
}

/// Concurrency cap and per-provider deadline for the tray's refresh.
const COLLECT_OPTIONS: CollectOptions = CollectOptions {
    max_concurrency: codexbar_core::refresh::DEFAULT_MAX_CONCURRENCY,
    provider_timeout: codexbar_core::refresh::DEFAULT_PROVIDER_TIMEOUT,
};

/// Run the `refreshCredentials` pass, or nothing at all.
///
/// `refreshCredentials` defaults to **false**, and while it is false the app
/// performs no credential write of any kind. The pass itself is honest about
/// this build having no provider-side hook yet (see `registry.rs`), so turning
/// the setting on never fabricates a "refreshed" result.
fn apply_credential_refresh<P>(settings: &Settings, providers: &[P]) {
    if !settings.refresh_credentials {
        return;
    }
    let CredentialRefreshOutcome {
        hook_available,
        refreshed,
    } = registry::refresh_credentials_pass(providers);
    if hook_available {
        eprintln!(
            "codexbar: refreshCredentials refreshed {} provider(s)",
            refreshed.len()
        );
    } else {
        eprintln!(
            "codexbar: refreshCredentials is on, but this build has no proactive credential-refresh \
             hook yet — nothing was written. Extension point: {}",
            registry::CREDENTIAL_REFRESH_ENTRY_POINT
        );
    }
}

/// One refresh cycle. **Runs on the refresh worker thread, never on the main
/// thread**: providers are fetched in parallel (bounded, with a deadline each),
/// transient failures keep the last good numbers as `stale`, a machine that
/// looks offline gets no retry storm, and only the finished report is handed
/// to the main thread for the tray repaint.
fn run_refresh_cycle(
    app: &AppHandle,
    backoff: &mut ProviderBackoff,
    manual: bool,
) -> (Arc<UsageReport>, RegistryMode) {
    let state = app.state::<AppState>();
    let settings = state.settings();
    let mode = RegistryMode::from_settings(&settings);
    let providers = state.registry.get(mode);
    apply_credential_refresh(&settings, providers.as_slice());

    let previous = state.snapshot();
    let started = Instant::now();
    let before = codexbar_providers::connectivity();
    // A provider backing off keeps its previous card for this cycle. A manual
    // refresh is the user asking "try again now", so it ignores the backoff.
    let skip = |id: ProviderId| -> Option<ProviderSnapshot> {
        if manual || !backoff.should_skip(id, started) {
            return None;
        }
        previous.get(id).cloned()
    };
    let result = collect_with(&providers, COLLECT_OPTIONS, chrono::Utc::now(), &skip);

    let delta = codexbar_providers::connectivity().since(before);
    let offline = mode == RegistryMode::Live && delta.looks_offline();
    // While offline the HTTP layer makes one attempt per request (no retry
    // loop); the first response of any kind clears it.
    codexbar_providers::set_offline_hint(offline);

    for outcome in &result.outcomes {
        if outcome.kind != OutcomeKind::Skipped {
            backoff.record(outcome.provider, outcome.failure(), started);
        }
    }
    let published = UsageReport::new(
        result
            .report
            .providers
            .into_iter()
            .map(|fresh| retain_last_good(Some(&previous), fresh, offline))
            .collect(),
    );
    let status = RefreshStatus::from_outcomes(
        &result.outcomes,
        &published,
        offline,
        backoff,
        Instant::now(),
    );
    if offline {
        eprintln!("codexbar: no network — keeping the last known values");
    }
    let report = Arc::new(published);
    state.store(Arc::clone(&report));
    state.set_refresh_status(status.clone());
    apply_refresh_on_main_thread(app, Arc::clone(&report), status);
    (report, mode)
}

/// Hand a finished report to the main thread: tray icons and menus belong to
/// it (tao/WebView2 fail when they are touched from another thread).
fn apply_refresh_on_main_thread(app: &AppHandle, report: Arc<UsageReport>, status: RefreshStatus) {
    let handle = app.clone();
    let dispatched = app.run_on_main_thread(move || {
        sync_trays(&handle);
        if let Err(err) = handle.emit_to(POPOVER, "usage-updated", &*report) {
            // Expected while the webview has not finished loading yet.
            eprintln!("codexbar: could not notify UI: {err}");
        }
        let _ = handle.emit_to(widget::WIDGET, "usage-updated", &*report);
        let _ = handle.emit_to(POPOVER, "refresh-status", &status);
        let _ = handle.emit_to(settings_window::SETTINGS_WINDOW, "refresh-status", &status);
    });
    if let Err(err) = dispatched {
        eprintln!("codexbar: could not schedule the tray update: {err}");
    }
}

/// Start the refresh worker thread and return its request channel.
///
/// The worker sleeps on the channel until the next refresh is due (no 1 s
/// polling), wakes early for a manual refresh or a settings change, and runs
/// every fetch itself — the main thread only ever receives finished reports.
/// It performs the first fetch immediately.
fn spawn_refresh_worker(app: &AppHandle) -> mpsc::Sender<RefreshRequest> {
    let (tx, rx) = mpsc::channel::<RefreshRequest>();
    let handle = app.clone();
    let spawned = std::thread::Builder::new()
        .name("codexbar-refresh".into())
        .spawn(move || {
            let mut backoff = ProviderBackoff::default();
            let (_, mut last_mode) = run_refresh_cycle(&handle, &mut backoff, true);
            let mut last = Instant::now();
            loop {
                let interval = handle.state::<AppState>().settings().refresh_interval_secs;
                let wait = time_until_due(last.elapsed(), interval);
                let request = match rx.recv_timeout(wait) {
                    Ok(request) => Some(request),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let (manual, reply) = match request {
                    None => {
                        if !refresh_due(last.elapsed(), interval) {
                            continue;
                        }
                        (false, None)
                    }
                    Some(RefreshRequest::Now { reply }) => {
                        handle.state::<AppState>().registry.invalidate();
                        (true, reply)
                    }
                    Some(RefreshRequest::SettingsChanged) => {
                        let settings = handle.state::<AppState>().settings();
                        if RegistryMode::from_settings(&settings) == last_mode {
                            continue; // new interval is picked up on the next wait
                        }
                        backoff.reset();
                        (true, None)
                    }
                };
                // Coalesce a burst of clicks into this one cycle.
                let mut replies: Vec<mpsc::Sender<Arc<UsageReport>>> = reply.into_iter().collect();
                while let Ok(extra) = rx.try_recv() {
                    if let RefreshRequest::Now { reply: Some(r) } = extra {
                        replies.push(r);
                    }
                }
                let (report, mode) = run_refresh_cycle(&handle, &mut backoff, manual);
                last_mode = mode;
                last = Instant::now();
                for reply in replies {
                    let _ = reply.send(Arc::clone(&report));
                }
            }
        });
    if let Err(err) = spawned {
        eprintln!("codexbar: could not start the refresh worker: {err}");
    }
    tx
}

// ---------------------------------------------------------------------------
// Offline CLI modes (evidence without a desktop session)
// ---------------------------------------------------------------------------

/// Overlay `patch` onto `base`: objects merge key by key, everything else is
/// replaced. Lets `--set-settings '{"mergeIcons":true}'` change one field
/// without wiping the rest of the file.
fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                merge_json(
                    base_map
                        .entry(key.clone())
                        .or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
        (slot, other) => *slot = other.clone(),
    }
}

/// Apply a JSON patch to the persisted settings, exactly like the
/// `set_settings` command does (same normalise + atomic write path).
fn apply_settings_json(patch: &str) -> Result<Settings, String> {
    let patch: serde_json::Value = serde_json::from_str(patch)
        .map_err(|e| format!("--set-settings expects a JSON object: {e}"))?;
    if !patch.is_object() {
        return Err("--set-settings expects a JSON object".to_string());
    }
    let mut base = serde_json::to_value(settings::load())
        .map_err(|e| format!("could not read the current settings: {e}"))?;
    merge_json(&mut base, &patch);
    let next: Settings = serde_json::from_value(base)
        .map_err(|e| format!("--set-settings has an invalid value: {e}"))?;
    let next = next.normalized();
    settings::save(&next)?;
    Ok(next)
}

/// `--dump-icons <dir>`: write every tray icon variant as PNG + an index.
///
/// This writes the *exact* RGBA buffers [`TrayIcon::set_icon`] receives, which
/// is the evidence that the percentage really is drawn into the bitmap.
fn dump_icons(dir: &str) -> std::io::Result<()> {
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;
    // Same mode the app is configured for: `--dump-icons` after
    // `--set-settings '{"mockMode":true}'` dumps sample-data icons, and the
    // default dumps the live ones (with the honest dash for every provider that
    // has no usable number this cycle).
    let settings = settings::load();
    let mode = RegistryMode::from_settings(&settings);
    let report = registry::report_for_settings(&settings);
    let mut index: Vec<serde_json::Value> = Vec::new();

    let mut write = |name: &str,
                     label: &str,
                     provider: Option<&str>,
                     pct: Option<f64>,
                     show_percent: bool|
     -> std::io::Result<()> {
        let rgba = tray_icon::icon_rgba(TRAY_PX, pct, show_percent);
        let png = png::encode_rgba(TRAY_PX, TRAY_PX, &rgba);
        let path = dir.join(name);
        std::fs::write(&path, &png)?;
        index.push(serde_json::json!({
            "file": name,
            "label": label,
            "provider": provider,
            "usedPercent": pct,
            "showPercentInIcon": show_percent,
            "size": TRAY_PX,
            "rgbaBytes": rgba.len(),
            "pngBytes": png.len(),
        }));
        Ok(())
    };

    // Per-provider icons exactly as the tray builds them.
    for snapshot in &report.providers {
        let pct = percent_of(&report, snapshot.provider);
        let name = format!("tray-{}.png", snapshot.provider.as_str());
        let label = provider_tooltip(&report, snapshot.provider);
        write(
            &name,
            &label,
            Some(snapshot.provider.as_str()),
            pct,
            settings.show_percent_in_icon,
        )?;
    }
    // Merged icon (worst of all enabled).
    let merged = merged_percent(&report, &settings);
    write(
        "tray-merged.png",
        &merged_tooltip(&report, &settings),
        None,
        merged,
        true,
    )?;
    // Same data with the percentage switched off — proves the toggle matters.
    write(
        "tray-codex-gauge-only.png",
        "Codex with showPercentInIcon=false",
        Some("codex"),
        percent_of(&report, ProviderId::Codex),
        false,
    )?;
    // A sweep of values so the drawn digits can be eyeballed.
    for pct in [0.0_f64, 7.0, 12.0, 42.0, 76.0, 90.0, 100.0] {
        let name = format!("tray-pct-{:03}.png", pct.round() as i64);
        write(
            &name,
            &format!("synthetic {pct:.0}%"),
            None,
            Some(pct),
            true,
        )?;
    }
    // Unknown / not-configured providers.
    write(
        "tray-unknown.png",
        "no usable number (dash)",
        None,
        None,
        true,
    )?;

    let index_path = dir.join("tray-icons.json");
    std::fs::write(
        &index_path,
        serde_json::to_string_pretty(&index).expect("index is serialisable"),
    )?;
    println!(
        "wrote {} icon PNGs ({mode} mode) + {} to {}",
        index.len(),
        index_path.display(),
        dir.display()
    );
    Ok(())
}

/// `--dump-report <file>`: write the exact payload the popover renders.
///
/// Same registry selection the background loop uses, so this file is the
/// machine-readable half of a screenshot: `mode` is the report mode, every
/// provider carries its own `status` / `error` / `source`, and provider workers
/// cannot fake a state the UI would not show. Credential values never appear —
/// providers mask them (`sk-or-1v…9f2c`).
fn dump_report(path: &str, mode: RegistryMode) -> std::io::Result<()> {
    let settings = settings::load();
    let providers = registry::registry_for(mode);
    apply_credential_refresh(&settings, &providers);
    let report = codexbar_core::collect(&providers);

    let path = PathBuf::from(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&report).expect("report is serialisable"),
    )?;

    println!("mode: {}", mode.as_str());
    println!(
        "providers: {} (schema v{})",
        report.providers.len(),
        report.schema_version
    );
    for snapshot in &report.providers {
        println!(
            "  {:<11} {:<14} {:<22} {}",
            snapshot.provider.as_str(),
            snapshot.status.as_str(),
            describe(snapshot),
            source_label(snapshot)
        );
    }
    println!("wrote {}", path.display());
    Ok(())
}

/// Human help for the CLI modes.
fn print_help() {
    println!(
        "codexbar-win {} — CodexBar tray app for Windows\n\n\
         USAGE:\n  codexbar-win [--show] [--settings] [--settings-builtin] [--dump-icons <dir>] [--dump-report <file>] \\\n\
         \x20              [--mock|--live] [--print-settings] [--set-settings <json>] \\\n\
         \x20              [--config-path] [--help]\n\n\
         FLAGS:\n  \
         --show              open the popover on start (screenshots, manual inspection)\n  \
         --settings          open the settings window on start (screenshots, manual inspection)\n  \
         --settings-builtin  open the settings window on the built-in page (evidence, no UI bundle)\n  \
         --dump-icons DIR    write the tray icon bitmaps as PNG into DIR, then exit\n  \
         --dump-report FILE  write the registry report the popover renders into FILE, then exit\n  \
         --mock, --live      force the report mode for --dump-report (default: config.json)\n  \
         --print-settings    print the effective settings JSON, then exit\n  \
         --set-settings JSON merge the JSON into config.json, apply it, then exit\n  \
         --config-path       print the settings file path, then exit\n  \
         --help, -h          this text\n\n\
         MODES:\n  \
         live (default)      real providers: local credentials are read, usage is fetched\n  \
         mock                sample data only: config.json {{\"mockMode\":true}}, no network\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let flag_value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }
    if args.iter().any(|a| a == "--config-path") {
        println!("{}", settings::config_path().display());
        return;
    }
    if args.iter().any(|a| a == "--print-settings") {
        let settings = settings::load();
        println!(
            "{}",
            serde_json::to_string_pretty(&settings).expect("settings are serialisable")
        );
        return;
    }
    if let Some(dir) = flag_value("--dump-icons") {
        if let Err(err) = dump_icons(&dir) {
            eprintln!("codexbar: --dump-icons failed: {err}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(path) = flag_value("--dump-report") {
        // `--mock` / `--live` force the mode for this one invocation; without
        // them the persisted `mockMode` decides, exactly like the tray loop.
        let requested = if args.iter().any(|a| a == "--mock") {
            Some(RegistryMode::Mock)
        } else if args.iter().any(|a| a == "--live") {
            Some(RegistryMode::Live)
        } else {
            None
        };
        let mode = requested.unwrap_or_else(|| RegistryMode::from_settings(&settings::load()));
        if let Err(err) = dump_report(&path, mode) {
            eprintln!("codexbar: --dump-report failed: {err}");
            std::process::exit(1);
        }
        return;
    }
    if let Some(json) = flag_value("--set-settings") {
        match apply_settings_json(&json) {
            Ok(next) => {
                autostart::reconcile(next.start_at_login);
                eprintln!(
                    "codexbar: settings saved to {}",
                    settings::config_path().display()
                );
                println!(
                    "{}",
                    serde_json::to_string_pretty(&next).expect("settings are serialisable")
                );
            }
            Err(err) => {
                eprintln!("codexbar: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    let show_on_start = args.iter().any(|arg| arg == "--show");
    let settings_on_start = args.iter().any(|arg| arg == "--settings");
    // Evidence seam: open the settings window on the self-contained built-in
    // page, bypassing the UI bundle.
    let settings_builtin = args.iter().any(|arg| arg == "--settings-builtin");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            widget::focus_existing(app, &args)
        }))
        .register_uri_scheme_protocol(
            settings_window::SETTINGS_SCHEME,
            settings_window::protocol_response,
        )
        // Global page-load hook: the popover is created from `tauri.conf.json`,
        // so this is where it reports that its first frame can be painted.
        .on_page_load(on_any_page_load)
        .invoke_handler(tauri::generate_handler![
            get_report,
            usage_snapshot,
            refresh_now,
            get_settings,
            set_settings,
            default_settings,
            settings_info,
            provider_login,
            copilot_login,
            copilot_login_start,
            copilot_login_poll,
            copilot_login_cancel,
            reauth_launch,
            reauth_status,
            reauth_cancel,
            session_states,
            refresh_status,
            open_settings,
            hide_settings,
            hide_popover,
            quit,
            quit_app,
            app_metadata,
            appearance::get_appearance,
            appearance::set_appearance,
            widget::toggle_widget,
            widget::widget_state,
            widget::widget_update,
            widget::widget_drag
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            let settings = settings::load();
            let mode = RegistryMode::from_settings(&settings);
            // Start with an empty report so a slow live provider can never delay
            // the tray or the popover: the first fetch runs below, after the
            // window exists, and the UI gets the real payload from `get_report`.
            app.manage(AppState::new(settings.clone()));
            app.manage(appearance::AppearanceState::load());

            widget::restore(&handle);
            install_trays(&handle)?;

            // Make the Run key agree with the persisted setting (idempotent).
            autostart::reconcile(settings.start_at_login);

            // `--settings`: open the settings window straight away (evidence,
            // manual inspection) instead of waiting for the tray menu. Without
            // either flag the window is pre-warmed hidden, so the first real
            // open is a reuse (no cold webview, no white frame).
            if settings_builtin {
                settings_window::open_builtin(&handle);
            } else if settings_on_start {
                settings_window::open(&handle);
            } else {
                settings_window::prewarm(&handle);
            }

            // Hide instead of closing, and dismiss when focus is lost (flyout feel).
            //
            // The handler is armed only after the window has genuinely received
            // focus once: `show()` + `set_focus()` during startup can emit a
            // spurious `Focused(false)` (the launching console still owns focus on
            // Windows) which would otherwise close the popover before it is ever
            // painted. `--show` skips the handler entirely so the window stays put
            // for screenshots and manual inspection.
            if let Some(win) = app.get_webview_window(POPOVER) {
                if !show_on_start {
                    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let win_for_events = win.clone();
                    let armed_for_events = armed.clone();
                    win.on_window_event(move |event| match event {
                        tauri::WindowEvent::Focused(true) => {
                            armed_for_events.store(true, std::sync::atomic::Ordering::SeqCst);
                        }
                        tauri::WindowEvent::Focused(false)
                            if armed_for_events
                                .swap(false, std::sync::atomic::Ordering::SeqCst) =>
                        {
                            let _ = win_for_events.hide();
                        }
                        _ => {}
                    });
                }
                if show_on_start {
                    show_popover(&handle, &win);
                }
            }

            // Refresh worker: does the first fetch right away and then one per
            // `refreshIntervalSecs`, all off the main thread, so a slow provider
            // can never delay the tray, the menus or the popover. Only the
            // finished report is dispatched back to the main thread, which
            // owns the tray images and menus.
            let worker = spawn_refresh_worker(&handle);
            *handle
                .state::<AppState>()
                .refresh_tx
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(worker);

            eprintln!(
                "codexbar-win {} ready — {} mode, {} providers, {} tray icon(s), config {}",
                env!("CARGO_PKG_VERSION"),
                mode.as_str(),
                ProviderId::ALL.len(),
                handle.state::<AppState>().tray_ids().len(),
                settings::config_path().display()
            );
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running CodexBar");
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn merge_json_overlays_objects_and_replaces_scalars() {
        let mut base = serde_json::json!({
            "refreshIntervalSecs": 300,
            "mergeIcons": false,
            "providers": [{ "id": "codex", "enabled": true }],
        });
        let patch = serde_json::json!({ "mergeIcons": true, "extra": 1 });
        merge_json(&mut base, &patch);
        assert_eq!(base["mergeIcons"], serde_json::json!(true));
        assert_eq!(base["refreshIntervalSecs"], serde_json::json!(300));
        assert_eq!(base["extra"], serde_json::json!(1));
        assert_eq!(base["providers"][0]["id"], serde_json::json!("codex"));
    }

    #[test]
    fn merge_json_replaces_arrays_wholesale() {
        let mut base = serde_json::json!({ "providers": [{ "id": "codex", "enabled": true }] });
        let patch = serde_json::json!({ "providers": [{ "id": "claude", "enabled": false }] });
        merge_json(&mut base, &patch);
        assert_eq!(base["providers"].as_array().unwrap().len(), 1);
        assert_eq!(base["providers"][0]["id"], serde_json::json!("claude"));
    }

    // ---- refresh cadence (`refreshIntervalSecs`) --------------------------

    /// The background gate is what makes `refreshIntervalSecs` real: a wake-up
    /// before the configured cadence must not fetch, one at/after it must.
    #[test]
    fn refresh_due_honours_the_configured_interval() {
        assert!(!refresh_due(Duration::from_secs(59), 60));
        assert!(refresh_due(Duration::from_secs(60), 60));
        assert!(refresh_due(Duration::from_secs(61), 60));

        // The default cadence (5 min) and the settings clamp (15 s) both work.
        assert!(!refresh_due(Duration::from_secs(299), 300));
        assert!(refresh_due(Duration::from_secs(300), 300));
        assert!(!refresh_due(Duration::from_secs(14), 15));
        assert!(refresh_due(Duration::from_secs(15), 15));

        // A hand-edited 0 can never turn the loop into a busy spin.
        assert!(!refresh_due(Duration::from_millis(500), 0));
        assert!(refresh_due(Duration::from_secs(1), 0));
    }

    /// The worker sleeps exactly until the next refresh is due, never negative.
    #[test]
    fn worker_sleeps_until_the_next_refresh() {
        assert_eq!(
            time_until_due(Duration::from_secs(10), 60),
            Duration::from_secs(50)
        );
        assert_eq!(time_until_due(Duration::from_secs(90), 60), Duration::ZERO);
        assert_eq!(time_until_due(Duration::ZERO, 0), Duration::from_secs(1));
    }

    // ---- tray reconciliation ----------------------------------------------

    /// A refresh with unchanged settings must update the trays in place, never
    /// remove and re-create them (that is what made the icons blink).
    #[test]
    fn unchanged_settings_update_trays_in_place() {
        let settings = Settings::default();
        let current = desired_tray_ids(&settings);
        assert_eq!(
            desired_tray_ids(&settings.clone()),
            current,
            "desired ids are deterministic"
        );
        assert_eq!(
            tray_plan(&current, &desired_tray_ids(&settings)),
            TrayPlan::UpdateInPlace
        );

        let merged = Settings {
            merge_icons: !settings.merge_icons,
            ..settings.clone()
        };
        assert_eq!(
            tray_plan(&current, &desired_tray_ids(&merged)),
            TrayPlan::Reinstall
        );
        assert_eq!(tray_plan(&[], &current), TrayPlan::Reinstall);
    }

    /// The menu signature changes exactly when the menu content would, so an
    /// unchanged report does not rebuild the menu.
    #[test]
    fn menu_signature_tracks_visible_content_only() {
        let settings = Settings::default();
        let enabled = settings.enabled_providers();
        let id = *enabled.first().expect("a provider is enabled by default");
        let make = |used: f64| {
            UsageReport::new(vec![snapshot(
                id,
                FetchStatus::Ok,
                None,
                DataSource::OAuth,
                Some(used),
            )])
        };
        let a = make(40.0);
        let mut b = make(40.0);
        b.generated_at = a.generated_at + chrono::Duration::minutes(5);
        assert_eq!(menu_signature(&a, &settings), menu_signature(&b, &settings));
        assert_ne!(
            menu_signature(&a, &settings),
            menu_signature(&make(41.0), &settings)
        );
        let toggled = Settings {
            show_percent_in_icon: !settings.show_percent_in_icon,
            ..settings.clone()
        };
        assert_ne!(menu_signature(&a, &settings), menu_signature(&a, &toggled));
    }

    #[test]
    fn icon_key_ignores_sub_decimal_noise() {
        assert_eq!(icon_key(Some(41.99), true), icon_key(Some(42.0), true));
        assert_ne!(icon_key(Some(41.0), true), icon_key(Some(42.0), true));
        assert_ne!(icon_key(Some(42.0), true), icon_key(Some(42.0), false));
        assert_ne!(icon_key(None, true), icon_key(Some(0.0), true));
    }

    /// The normalized settings are what the loop reads, so the clamp is part of
    /// the contract too.
    #[test]
    fn normalized_settings_bound_the_interval_the_loop_reads() {
        let raw = Settings {
            refresh_interval_secs: 1,
            ..Settings::default()
        }
        .normalized();
        assert_eq!(raw.refresh_interval_secs, settings::MIN_REFRESH_SECS);
        assert!(!refresh_due(
            Duration::from_secs(14),
            raw.refresh_interval_secs
        ));
        assert!(refresh_due(
            Duration::from_secs(15),
            raw.refresh_interval_secs
        ));
    }

    /// `apply_settings` skips the write when nothing changed, which is only
    /// sound because normalisation is idempotent: a settings object that has
    /// already been normalised must normalise to itself, otherwise every save
    /// would look like a change and the guard would never fire.
    #[test]
    fn normalized_settings_are_stable_so_the_noop_save_guard_holds() {
        let once = Settings {
            refresh_interval_secs: 1,
            merge_icons: true,
            ..Settings::default()
        }
        .normalized();
        assert_eq!(once.clone().normalized(), once);
    }

    // ---- honest states ----------------------------------------------------

    fn snapshot(
        id: ProviderId,
        status: FetchStatus,
        error: Option<&str>,
        source: DataSource,
        used: Option<f64>,
    ) -> ProviderSnapshot {
        use codexbar_core::{NamedRateWindow, RateWindow, WindowKind};
        ProviderSnapshot {
            provider: id,
            title: id.title().to_string(),
            account: None,
            plan: None,
            windows: used
                .map(|pct| {
                    vec![NamedRateWindow::new(
                        "session",
                        "Session · 5h",
                        WindowKind::Session,
                        RateWindow::new(pct, Some(300), None),
                    )]
                })
                .unwrap_or_default(),
            balance: None,
            status,
            error: error.map(str::to_string),
            source,
            fetched_at: chrono::Utc::now(),
        }
    }

    /// An expired credential must read as "token expired" in the tray, using the
    /// same prose signal `ui/format.js` classifies on.
    #[test]
    fn token_expired_is_named_not_hidden_behind_a_generic_error() {
        let expired = snapshot(
            ProviderId::Codex,
            FetchStatus::Error,
            Some("OAuth token expired — sign in again with `codex login`."),
            DataSource::OAuth,
            None,
        );
        assert_eq!(describe(&expired), "token expired");

        let revoked = snapshot(
            ProviderId::Claude,
            FetchStatus::Error,
            Some("refresh failed: invalid_grant (token revoked)"),
            DataSource::OAuth,
            None,
        );
        assert_eq!(describe(&revoked), "token expired");

        let other = snapshot(
            ProviderId::Groq,
            FetchStatus::Error,
            Some("groq returned HTTP 500"),
            DataSource::ApiKey,
            None,
        );
        assert_eq!(describe(&other), "fetch failed");
    }

    /// Every status has a truthful one-liner — and an unknown number is never
    /// rendered as `0%`.
    #[test]
    fn describe_never_invents_a_percentage() {
        let not_configured = snapshot(
            ProviderId::Gemini,
            FetchStatus::NotConfigured,
            Some("set GEMINI_API_KEY"),
            DataSource::OAuth,
            None,
        );
        assert_eq!(describe(&not_configured), "not configured");

        let empty = snapshot(
            ProviderId::Kimi,
            FetchStatus::Ok,
            None,
            DataSource::ApiKey,
            None,
        );
        assert_eq!(describe(&empty), "no quota window");

        let ok = snapshot(
            ProviderId::OpenRouter,
            FetchStatus::Ok,
            None,
            DataSource::ApiKey,
            Some(42.4),
        );
        assert_eq!(describe(&ok), "42% used");

        let stale = snapshot(
            ProviderId::OpenRouter,
            FetchStatus::Stale,
            None,
            DataSource::ApiKey,
            Some(7.0),
        );
        assert_eq!(describe(&stale), "7% used (stale)");
    }

    /// The source half of the tooltip: sample data vs live path vs nothing.
    #[test]
    fn source_label_separates_sample_from_live() {
        let mock = snapshot(
            ProviderId::Codex,
            FetchStatus::Ok,
            None,
            DataSource::Mock,
            Some(10.0),
        );
        assert_eq!(source_label(&mock), "sample data");

        let live = snapshot(
            ProviderId::Codex,
            FetchStatus::Ok,
            None,
            DataSource::OAuth,
            Some(10.0),
        );
        assert_eq!(source_label(&live), "live/oauth");

        let report = UsageReport::new(vec![mock]);
        assert_eq!(
            provider_tooltip(&report, ProviderId::Codex),
            "Codex — 10% used · sample data"
        );
        let live_report = UsageReport::new(vec![snapshot(
            ProviderId::Codex,
            FetchStatus::Error,
            Some("token expired"),
            DataSource::OAuth,
            None,
        )]);
        assert_eq!(
            provider_tooltip(&live_report, ProviderId::Codex),
            "Codex — token expired · live/oauth"
        );
        // A provider that never reported at all still says which mode it is in.
        assert!(provider_tooltip(&live_report, ProviderId::Groq).contains("no data"));
    }

    /// The merged tooltip is the one-line "is this sample data?" answer, and it
    /// must stay inside the Windows tooltip budget.
    #[test]
    fn merged_tooltip_tags_sample_data_and_stays_short() {
        let sample_settings = Settings {
            mock_mode: true,
            ..Settings::default()
        };
        let mock_report = registry::report_for(RegistryMode::Mock);
        let mock_text = merged_tooltip(&mock_report, &sample_settings);
        assert!(mock_text.ends_with(" · sample data"), "{mock_text}");
        assert!(
            mock_text.len() <= 127,
            "{} chars: {mock_text}",
            mock_text.len()
        );

        // Live mode has no tag; with no live numbers it says so plainly.
        let empty = UsageReport::new(vec![]);
        let live_text = merged_tooltip(&empty, &Settings::default());
        assert_eq!(live_text, "CodexBar — no provider data");

        // A live report picks up the live numbers and no sample tag.
        let live_report = UsageReport::new(vec![snapshot(
            ProviderId::Codex,
            FetchStatus::Ok,
            None,
            DataSource::OAuth,
            Some(61.0),
        )]);
        let live_number_text = merged_tooltip(&live_report, &Settings::default());
        assert_eq!(live_number_text, "Codex 61%");
        assert!(!live_number_text.contains("sample data"));
    }

    /// Mock mode carries no live sources at all — the report the tray serves in
    /// sample mode cannot leak a real fetch path.
    #[test]
    fn sample_mode_report_is_labelled_everywhere() {
        let report = registry::report_for(RegistryMode::Mock);
        for provider in &report.providers {
            assert_eq!(source_label(provider), "sample data");
            assert!(provider_tooltip(&report, provider.provider).contains("sample data"));
        }
    }
}
