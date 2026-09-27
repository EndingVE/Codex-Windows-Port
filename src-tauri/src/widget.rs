//! Floating desktop widget: one compact row per active provider.
//!
//! The widget is a separate `widget` webview window, created on demand with
//! [`WebviewWindowBuilder`] (it is deliberately *not* declared in
//! `tauri.conf.json`, so a user who never opens it pays nothing for it). It is
//! frameless, transparent, resizable, kept out of the taskbar and — by default —
//! always on top. It never fetches anything itself: it renders the same
//! `UsageReport` the popover gets (`get_report` + the `usage-updated` event).
//!
//! Its state lives in `%APPDATA%\CodexBar\widget.json`, next to `config.json`:
//!
//! * `visible`, `alwaysOnTop`, `locked`, `clickThrough` and an optional global
//!   `hotkey` (e.g. `"Ctrl+Alt+W"`, off by default);
//! * `x`/`y` — the outer position in **physical** virtual-desktop pixels. That is
//!   the coordinate space Windows uses for monitor rectangles, so a saved point
//!   can be validated against the current monitor layout without guessing which
//!   monitor's scale factor it was converted with;
//! * `width`/`height` — the inner size in **logical** pixels, so the widget keeps
//!   the same apparent size when the DPI scale of its monitor changes.
//!
//! Writes are atomic (`widget.json.tmp` → rename) and debounced: a drag emits
//! dozens of `Moved` events, but a single saver thread only writes once the
//! window has been still for [`SAVE_DEBOUNCE`].
//!
//! On restore the saved position is checked against the work areas of the
//! monitors that exist *now* ([`clamp_to_monitors`]); if the monitor it lived
//! on has been unplugged, the widget is re-parked on the primary monitor.
//!
//! "Locked" disables dragging and resizing; "click-through" makes the window
//! ignore the mouse entirely (`set_ignore_cursor_events`). Both — and the
//! visibility itself — can always be undone from the tray submenu built by
//! [`submenu`], because a click-through window cannot be clicked.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::{
    menu::{CheckMenuItem, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    WindowEvent, Wry,
};

/// Window label. Also listed in `capabilities/default.json` so the page may
/// listen to `usage-updated` (see the note in that file).
pub const WIDGET: &str = "widget";
/// State file, stored next to `config.json`.
pub const STATE_FILE: &str = "widget.json";
/// Command-line flag that forces the widget visible at start (and, sent to a
/// running instance, shows it).
pub const SHOW_FLAG: &str = "--widget";
/// Prefix of every tray menu id this module owns.
pub const MENU_PREFIX: &str = "widget:";

const MENU_TOGGLE: &str = "widget:toggle";
const MENU_ON_TOP: &str = "widget:top";
const MENU_LOCK: &str = "widget:lock";
const MENU_CLICK_THROUGH: &str = "widget:click";
const MENU_RESET: &str = "widget:reset";

/// Quiet period before a moved/resized widget is written to disk.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);

const DEFAULT_WIDTH: f64 = 300.0;
const DEFAULT_HEIGHT: f64 = 196.0;
const MIN_WIDTH: f64 = 200.0;
const MIN_HEIGHT: f64 = 72.0;
const MAX_WIDTH: f64 = 1600.0;
const MAX_HEIGHT: f64 = 1600.0;
/// Distance from the work-area corner when the widget is (re)parked, logical px.
const PARK_MARGIN: f64 = 24.0;
/// How much of the widget's top strip (its grip) must be on a monitor for a
/// saved position to be kept as is, physical px.
const MIN_VISIBLE_W: i32 = 64;
const MIN_VISIBLE_H: i32 = 24;

// ---------------------------------------------------------------------------
// Persisted state (pure)
// ---------------------------------------------------------------------------

/// Everything `widget.json` remembers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WidgetState {
    pub visible: bool,
    /// Outer position, physical virtual-desktop pixels. `None` = never placed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    /// Inner size, logical pixels.
    pub width: f64,
    pub height: f64,
    pub always_on_top: bool,
    pub locked: bool,
    pub click_through: bool,
    /// Optional global shortcut that toggles the widget, e.g. `Ctrl+Alt+W`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hotkey: Option<String>,
}

impl Default for WidgetState {
    fn default() -> Self {
        Self {
            visible: false,
            x: None,
            y: None,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            always_on_top: true,
            locked: false,
            click_through: false,
            hotkey: None,
        }
    }
}

impl WidgetState {
    /// Repair values a hand-edited (or older) file may carry.
    pub fn normalized(mut self) -> Self {
        let fix = |v: f64, default: f64, min: f64, max: f64| {
            if v.is_finite() && v > 0.0 {
                v.clamp(min, max)
            } else {
                default
            }
        };
        self.width = fix(self.width, DEFAULT_WIDTH, MIN_WIDTH, MAX_WIDTH);
        self.height = fix(self.height, DEFAULT_HEIGHT, MIN_HEIGHT, MAX_HEIGHT);
        // A position is only meaningful as a pair.
        if self.x.is_none() || self.y.is_none() {
            self.x = None;
            self.y = None;
        }
        self.hotkey = self
            .hotkey
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty());
        self
    }
}

/// Partial update sent by the widget page (`widget_update`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WidgetPatch {
    pub visible: Option<bool>,
    pub always_on_top: Option<bool>,
    pub locked: Option<bool>,
    pub click_through: Option<bool>,
}

impl WidgetPatch {
    pub fn apply(&self, state: &mut WidgetState) {
        if let Some(v) = self.visible {
            state.visible = v;
        }
        if let Some(v) = self.always_on_top {
            state.always_on_top = v;
        }
        if let Some(v) = self.locked {
            state.locked = v;
        }
        if let Some(v) = self.click_through {
            state.click_through = v;
        }
    }
}

/// Parse `widget.json` text. Anything unreadable yields the defaults (hidden),
/// so a corrupt file can never keep the app from starting.
pub fn parse_state(text: &str) -> WidgetState {
    serde_json::from_str::<WidgetState>(text)
        .map(WidgetState::normalized)
        .unwrap_or_default()
}

/// Read the state file at `path`; missing or corrupt → defaults.
pub fn load_from(path: &Path) -> WidgetState {
    match fs::read_to_string(path) {
        Ok(text) => {
            let state = parse_state(&text);
            if serde_json::from_str::<serde_json::Value>(&text).is_err() {
                eprintln!(
                    "codexbar: {} is not valid JSON, using widget defaults",
                    path.display()
                );
            }
            state
        }
        Err(_) => WidgetState::default(),
    }
}

/// Write `state` to `path` atomically (temp file + rename).
pub fn save_to(path: &Path, state: &WidgetState) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    }
    let json = serde_json::to_string_pretty(state)
        .map_err(|e| format!("could not serialise widget state: {e}"))?;
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

/// `%APPDATA%\CodexBar\widget.json`.
pub fn state_path() -> PathBuf {
    crate::settings::config_dir().join(STATE_FILE)
}

// ---------------------------------------------------------------------------
// Multi-monitor placement (pure)
// ---------------------------------------------------------------------------

/// A rectangle in physical virtual-desktop pixels (may have negative origin:
/// a monitor to the left of / above the primary one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    fn right(&self) -> i32 {
        self.x.saturating_add(self.w)
    }

    fn bottom(&self) -> i32 {
        self.y.saturating_add(self.h)
    }

    /// Size of the overlap with `other` (0×0 when disjoint).
    fn overlap(&self, other: &Rect) -> (i32, i32) {
        let w = self.right().min(other.right()) - self.x.max(other.x);
        let h = self.bottom().min(other.bottom()) - self.y.max(other.y);
        (w.max(0), h.max(0))
    }
}

/// Default spot: top-right corner of `area`, `margin` px inside.
pub fn park_position(area: Rect, size: (i32, i32), margin: i32) -> (i32, i32) {
    let x = area.right() - size.0 - margin;
    let y = area.y + margin;
    (x.max(area.x), y.max(area.y))
}

/// Validate a saved outer position against the monitors that exist now.
///
/// * `saved` — the persisted position (`None` when never placed);
/// * `size` — the widget's outer size in physical pixels;
/// * `monitors` — work areas of every current monitor, physical pixels;
/// * `primary` — index of the primary monitor in `monitors`.
///
/// The position is kept when the widget's grip strip (its top
/// [`MIN_VISIBLE_H`] px) overlaps some monitor by at least [`MIN_VISIBLE_W`]
/// px, and is then nudged so the whole widget fits that monitor when it can.
/// Otherwise — typically because the monitor it lived on was disconnected —
/// the widget is parked in the top-right corner of the primary monitor.
pub fn clamp_to_monitors(
    saved: Option<(i32, i32)>,
    size: (i32, i32),
    monitors: &[Rect],
    primary: usize,
    margin: i32,
) -> (i32, i32) {
    let fallback_area = monitors
        .get(primary)
        .or_else(|| monitors.first())
        .copied()
        .unwrap_or(Rect::new(0, 0, 1280, 720));
    let Some((x, y)) = saved else {
        return park_position(fallback_area, size, margin);
    };

    let grip = Rect::new(x, y, size.0.max(1), MIN_VISIBLE_H.min(size.1.max(1)));
    let best = monitors
        .iter()
        .map(|m| (m, grip.overlap(m)))
        .filter(|(_, (w, h))| *w >= MIN_VISIBLE_W.min(size.0.max(1)) && *h > 0)
        .max_by_key(|(_, (w, h))| i64::from(*w) * i64::from(*h))
        .map(|(m, _)| *m);

    match best {
        Some(area) => {
            // Pull it fully inside the monitor it mostly sits on (when it fits).
            let nx = if size.0 <= area.w {
                x.clamp(area.x, area.right() - size.0)
            } else {
                area.x
            };
            let ny = if size.1 <= area.h {
                y.clamp(area.y, area.bottom() - size.1)
            } else {
                area.y
            };
            (nx, ny)
        }
        None => park_position(fallback_area, size, margin),
    }
}

// ---------------------------------------------------------------------------
// Global hotkey (pure parser + Win32 RegisterHotKey)
// ---------------------------------------------------------------------------

const MOD_ALT: u32 = 0x0001;
const MOD_CONTROL: u32 = 0x0002;
const MOD_SHIFT: u32 = 0x0004;
const MOD_WIN: u32 = 0x0008;

/// Parse `Ctrl+Alt+W` into `(modifiers, virtual key)` for `RegisterHotKey`.
///
/// Accepts letters, digits and F1-F24, and requires at least one modifier so a
/// bare key can never be swallowed system-wide.
pub fn parse_hotkey(spec: &str) -> Result<(u32, u32), String> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in spec.split('+').map(str::trim) {
        if part.is_empty() {
            return Err(format!("invalid hotkey {spec:?}: empty part"));
        }
        let upper = part.to_ascii_uppercase();
        match upper.as_str() {
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL,
            "ALT" => mods |= MOD_ALT,
            "SHIFT" => mods |= MOD_SHIFT,
            "WIN" | "SUPER" | "META" => mods |= MOD_WIN,
            other => {
                if key.is_some() {
                    return Err(format!("invalid hotkey {spec:?}: more than one key"));
                }
                let bytes = other.as_bytes();
                key = Some(if bytes.len() == 1 && bytes[0].is_ascii_alphanumeric() {
                    u32::from(bytes[0])
                } else if let Some(n) = other
                    .strip_prefix('F')
                    .and_then(|n| n.parse::<u32>().ok())
                    .filter(|n| (1..=24).contains(n))
                {
                    0x70 + n - 1
                } else {
                    return Err(format!("invalid hotkey {spec:?}: unknown key {part:?}"));
                });
            }
        }
    }
    match key {
        Some(_) if mods == 0 => Err(format!("invalid hotkey {spec:?}: needs a modifier")),
        Some(vk) => Ok((mods, vk)),
        None => Err(format!("invalid hotkey {spec:?}: no key")),
    }
}

#[cfg(windows)]
mod hotkey_win {
    use std::ffi::c_void;

    const WM_HOTKEY: u32 = 0x0312;
    const MOD_NOREPEAT: u32 = 0x4000;

    #[repr(C)]
    struct Msg {
        hwnd: *mut c_void,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        pt_x: i32,
        pt_y: i32,
        private: u32,
    }

    #[link(name = "user32")]
    extern "system" {
        fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, vk: u32) -> i32;
        fn GetMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32) -> i32;
    }

    /// Register a thread hotkey and call `on_press` for every press. Runs on a
    /// dedicated thread (a thread hotkey is delivered to the registering
    /// thread's message queue).
    pub fn spawn(mods: u32, vk: u32, on_press: impl Fn() + Send + 'static) {
        std::thread::spawn(move || {
            // SAFETY: null HWND registers a thread hotkey; arguments are plain ints.
            let ok = unsafe { RegisterHotKey(std::ptr::null_mut(), 1, mods | MOD_NOREPEAT, vk) };
            if ok == 0 {
                eprintln!("codexbar: widget hotkey is already taken by another app");
                return;
            }
            let mut msg = Msg {
                hwnd: std::ptr::null_mut(),
                message: 0,
                wparam: 0,
                lparam: 0,
                time: 0,
                pt_x: 0,
                pt_y: 0,
                private: 0,
            };
            // SAFETY: `msg` is a valid, writable MSG-layout struct for the call.
            while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
                if msg.message == WM_HOTKEY {
                    on_press();
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Runtime (Tauri)
// ---------------------------------------------------------------------------

/// Managed state: the live [`WidgetState`] plus the debounced saver.
pub struct WidgetManager {
    state: Mutex<WidgetState>,
    saver: Mutex<Sender<()>>,
}

impl WidgetManager {
    fn new(state: WidgetState) -> Self {
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            // Block until the first change, then wait for a quiet period.
            while rx.recv().is_ok() {
                loop {
                    match rx.recv_timeout(SAVE_DEBOUNCE) {
                        Ok(()) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                SAVE_REQUEST.with_state(|s| {
                    if let Err(err) = save_to(&state_path(), &s) {
                        eprintln!("codexbar: could not save widget state: {err}");
                    }
                });
            }
        });
        Self {
            state: Mutex::new(state),
            saver: Mutex::new(tx),
        }
    }

    fn get(&self) -> WidgetState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn update(&self, f: impl FnOnce(&mut WidgetState)) -> WidgetState {
        let snapshot = match self.state.lock() {
            Ok(mut s) => {
                f(&mut s);
                s.clone()
            }
            Err(_) => return WidgetState::default(),
        };
        SAVE_REQUEST.set(snapshot.clone());
        if let Ok(tx) = self.saver.lock() {
            let _ = tx.send(());
        }
        snapshot
    }
}

/// Latest snapshot handed to the saver thread (it has no `AppHandle`).
struct PendingSave(Mutex<Option<WidgetState>>);

impl PendingSave {
    fn set(&self, state: WidgetState) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(state);
        }
    }

    fn with_state(&self, f: impl FnOnce(WidgetState)) {
        let taken = self.0.lock().ok().and_then(|mut slot| slot.take());
        if let Some(state) = taken {
            f(state);
        }
    }
}

static SAVE_REQUEST: PendingSave = PendingSave(Mutex::new(None));

fn manager(app: &AppHandle) -> Option<tauri::State<'_, WidgetManager>> {
    app.try_state::<WidgetManager>()
}

/// Called once from `setup`: load `widget.json`, manage the state, register the
/// optional hotkey and show the widget if it was visible (or `--widget` was
/// passed, e.g. from the autostart entry).
pub fn restore(app: &AppHandle) {
    let mut state = load_from(&state_path());
    if std::env::args().any(|a| a == SHOW_FLAG) {
        state.visible = true;
    }
    let visible = state.visible;
    let hotkey = state.hotkey.clone();
    app.manage(WidgetManager::new(state));

    if let Some(spec) = hotkey {
        register_hotkey(app, &spec);
    }
    if visible {
        show(app);
    }
}

#[cfg(windows)]
fn register_hotkey(app: &AppHandle, spec: &str) {
    match parse_hotkey(spec) {
        Ok((mods, vk)) => {
            let handle = app.clone();
            hotkey_win::spawn(mods, vk, move || {
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || toggle(&h));
            });
        }
        Err(err) => eprintln!("codexbar: {err}"),
    }
}

#[cfg(not(windows))]
fn register_hotkey(_app: &AppHandle, spec: &str) {
    if let Err(err) = parse_hotkey(spec) {
        eprintln!("codexbar: {err}");
    }
}

/// Current monitor work areas + primary index, physical pixels.
fn monitor_layout(app: &AppHandle) -> (Vec<Rect>, usize, f64) {
    let monitors = app.available_monitors().unwrap_or_default();
    let primary = app.primary_monitor().ok().flatten();
    let rects: Vec<Rect> = monitors
        .iter()
        .map(|m| {
            let a = m.work_area();
            Rect::new(
                a.position.x,
                a.position.y,
                a.size.width as i32,
                a.size.height as i32,
            )
        })
        .collect();
    let primary_idx = primary
        .as_ref()
        .and_then(|p| {
            monitors
                .iter()
                .position(|m| m.position() == p.position() && m.size() == p.size())
        })
        .unwrap_or(0);
    let scale = primary.map(|p| p.scale_factor()).unwrap_or(1.0);
    (rects, primary_idx, scale)
}

fn physical_size(state: &WidgetState, scale: f64) -> (i32, i32) {
    (
        (state.width * scale).round() as i32,
        (state.height * scale).round() as i32,
    )
}

/// Put the window where the saved state says, validated against the monitors.
fn place(app: &AppHandle, win: &WebviewWindow) {
    let Some(mgr) = manager(app) else {
        return;
    };
    let state = mgr.get();
    let (monitors, primary, primary_scale) = monitor_layout(app);
    // Outer size in physical px: prefer the real one once the window exists.
    let size = win
        .outer_size()
        .ok()
        .filter(|s| s.width > 0 && s.height > 0)
        .map(|s| (s.width as i32, s.height as i32))
        .unwrap_or_else(|| physical_size(&state, primary_scale));
    let margin = (PARK_MARGIN * primary_scale).round() as i32;
    let saved = state.x.zip(state.y);
    let (x, y) = clamp_to_monitors(saved, size, &monitors, primary, margin);
    let _ = win.set_position(PhysicalPosition::new(x, y));
    if saved != Some((x, y)) {
        mgr.update(|s| {
            s.x = Some(x);
            s.y = Some(y);
        });
    }
}

/// Apply always-on-top / locked / click-through to a live window.
fn apply_flags(win: &WebviewWindow, state: &WidgetState) {
    let _ = win.set_always_on_top(state.always_on_top);
    let _ = win.set_resizable(!state.locked);
    let _ = win.set_ignore_cursor_events(state.click_through);
}

fn build(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    let state = manager(app).map(|m| m.get()).unwrap_or_default();
    let win = WebviewWindowBuilder::new(app, WIDGET, WebviewUrl::App("widget.html".into()))
        .title("CodexBar Widget")
        .inner_size(state.width, state.height)
        .min_inner_size(MIN_WIDTH, MIN_HEIGHT)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .skip_taskbar(true)
        .always_on_top(state.always_on_top)
        .resizable(!state.locked)
        .focused(false)
        .visible(false)
        .build()?;

    let handle = app.clone();
    let win_for_events = win.clone();
    win.on_window_event(move |event| match event {
        WindowEvent::Moved(pos) => {
            if win_for_events.is_visible().unwrap_or(false) {
                if let Some(mgr) = manager(&handle) {
                    let (x, y) = (pos.x, pos.y);
                    mgr.update(|s| {
                        s.x = Some(x);
                        s.y = Some(y);
                    });
                }
            }
        }
        WindowEvent::Resized(size) => {
            // Minimising reports 0×0: not a size worth remembering.
            if size.width == 0 || size.height == 0 {
                return;
            }
            let scale = win_for_events.scale_factor().unwrap_or(1.0);
            if let Some(mgr) = manager(&handle) {
                let (w, h) = (size.width as f64 / scale, size.height as f64 / scale);
                mgr.update(|s| {
                    s.width = w.clamp(MIN_WIDTH, MAX_WIDTH);
                    s.height = h.clamp(MIN_HEIGHT, MAX_HEIGHT);
                });
            }
        }
        WindowEvent::CloseRequested { api, .. } => {
            // Never destroy: hide and remember it.
            api.prevent_close();
            hide(&handle);
        }
        _ => {}
    });
    Ok(win)
}

/// Show the widget (creating it on first use) without stealing focus.
pub fn show(app: &AppHandle) {
    let win = match app.get_webview_window(WIDGET) {
        Some(win) => win,
        None => match build(app) {
            Ok(win) => win,
            Err(err) => {
                eprintln!("codexbar: could not create the widget window: {err}");
                return;
            }
        },
    };
    let state = manager(app)
        .map(|m| m.update(|s| s.visible = true))
        .unwrap_or_default();
    place(app, &win);
    apply_flags(&win, &state);
    let _ = win.show();
}

/// Hide the widget and remember that it is hidden.
pub fn hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(WIDGET) {
        let _ = win.hide();
    }
    if let Some(mgr) = manager(app) {
        mgr.update(|s| s.visible = false);
    }
}

/// Tray / hotkey entry point.
pub fn toggle(app: &AppHandle) {
    let visible = app
        .get_webview_window(WIDGET)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if visible {
        hide(app);
    } else {
        show(app);
    }
}

/// Change state from the page or the tray and apply it to the live window.
fn apply_patch(app: &AppHandle, patch: &WidgetPatch) -> WidgetState {
    let Some(mgr) = manager(app) else {
        return WidgetState::default();
    };
    let state = mgr.update(|s| patch.apply(s));
    if let Some(win) = app.get_webview_window(WIDGET) {
        apply_flags(&win, &state);
    }
    match patch.visible {
        Some(true) => show(app),
        Some(false) => hide(app),
        None => {}
    }
    mgr.get()
}

/// Second launch (single-instance plugin): bring the running instance forward.
///
/// `--widget` (or an already visible widget) shows and focuses the widget;
/// otherwise the popover is shown, so a double-clicked shortcut always
/// produces something visible instead of a silent no-op.
pub fn focus_existing(app: &AppHandle, args: &[String]) {
    let widget_visible = app
        .get_webview_window(WIDGET)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if args.iter().any(|a| a == SHOW_FLAG) || widget_visible {
        show(app);
        if let Some(win) = app.get_webview_window(WIDGET) {
            let _ = win.set_focus();
        }
        return;
    }
    if let Some(win) = app.get_webview_window("popover") {
        let _ = win.show();
        let _ = win.set_focus();
    }
}

// ---------------------------------------------------------------------------
// Tray submenu
// ---------------------------------------------------------------------------

/// "Desktop widget ▸" submenu for the tray menu. Every mode that could make
/// the widget unreachable (click-through, locked) can be turned off here.
pub fn submenu(app: &AppHandle) -> tauri::Result<Submenu<Wry>> {
    let state = manager(app).map(|m| m.get()).unwrap_or_default();
    let visible = app
        .get_webview_window(WIDGET)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    let menu = Submenu::with_id(app, "widget", "Desktop widget", true)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        MENU_TOGGLE,
        "Show widget",
        true,
        visible,
        state.hotkey.as_deref(),
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        MENU_ON_TOP,
        "Always on top",
        true,
        state.always_on_top,
        None::<&str>,
    )?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        MENU_LOCK,
        "Lock position",
        true,
        state.locked,
        None::<&str>,
    )?)?;
    menu.append(&CheckMenuItem::with_id(
        app,
        MENU_CLICK_THROUGH,
        "Click-through",
        true,
        state.click_through,
        None::<&str>,
    )?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        MENU_RESET,
        "Reset widget position",
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

/// Handle a `widget:*` tray menu id. The caller rebuilds the tray menus
/// afterwards so the check marks follow.
pub fn handle_menu(app: &AppHandle, id: &str) {
    let current = manager(app).map(|m| m.get()).unwrap_or_default();
    match id {
        MENU_TOGGLE => toggle(app),
        MENU_ON_TOP => {
            apply_patch(
                app,
                &WidgetPatch {
                    always_on_top: Some(!current.always_on_top),
                    ..WidgetPatch::default()
                },
            );
        }
        MENU_LOCK => {
            apply_patch(
                app,
                &WidgetPatch {
                    locked: Some(!current.locked),
                    ..WidgetPatch::default()
                },
            );
        }
        MENU_CLICK_THROUGH => {
            apply_patch(
                app,
                &WidgetPatch {
                    click_through: Some(!current.click_through),
                    ..WidgetPatch::default()
                },
            );
        }
        MENU_RESET => {
            if let Some(mgr) = manager(app) {
                mgr.update(|s| {
                    s.x = None;
                    s.y = None;
                });
            }
            if let Some(win) = app.get_webview_window(WIDGET) {
                place(app, &win);
            }
        }
        _ => {}
    }
    notify_page(app);
}

fn notify_page(app: &AppHandle) {
    use tauri::Emitter;
    if let Some(mgr) = manager(app) {
        let _ = app.emit_to(WIDGET, "widget-state", mgr.get());
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Show/hide the widget; returns the new visibility.
#[tauri::command]
pub fn toggle_widget(app: AppHandle) -> bool {
    toggle(&app);
    app.get_webview_window(WIDGET)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false)
}

/// Persisted widget state, for the page's pin/lock buttons.
#[tauri::command]
pub fn widget_state(app: AppHandle) -> WidgetState {
    manager(&app).map(|m| m.get()).unwrap_or_default()
}

/// Partial update from the page (pin, lock, click-through, hide).
#[tauri::command]
pub fn widget_update(app: AppHandle, patch: WidgetPatch) -> WidgetState {
    let state = apply_patch(&app, &patch);
    notify_page(&app);
    state
}

/// Start a native drag from the page's grip, unless the widget is locked.
#[tauri::command]
pub fn widget_drag(app: AppHandle) -> bool {
    let locked = manager(&app).map(|m| m.get().locked).unwrap_or(false);
    if locked {
        return false;
    }
    app.get_webview_window(WIDGET)
        .map(|w| w.start_dragging().is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "codexbar-widget-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join(STATE_FILE)
    }

    #[test]
    fn defaults_are_hidden_on_top_and_unplaced() {
        let s = WidgetState::default();
        assert!(!s.visible);
        assert!(s.always_on_top);
        assert!(!s.locked);
        assert!(!s.click_through);
        assert_eq!((s.x, s.y), (None, None));
        assert!(s.hotkey.is_none());
    }

    #[test]
    fn widget_json_round_trips() {
        let path = temp_file("roundtrip");
        let state = WidgetState {
            visible: true,
            x: Some(-1500),
            y: Some(40),
            width: 320.0,
            height: 180.0,
            always_on_top: false,
            locked: true,
            click_through: true,
            hotkey: Some("Ctrl+Alt+W".into()),
        };
        save_to(&path, &state).unwrap();
        assert_eq!(load_from(&path), state);
        // Atomic write leaves no temp file behind.
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        assert!(!tmp.exists());
        // camelCase on disk.
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"alwaysOnTop\": false"));
        assert!(text.contains("\"clickThrough\": true"));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn corrupt_json_falls_back_to_hidden_defaults() {
        let path = temp_file("corrupt");
        fs::write(&path, b"{ \"visible\": true, \"x\": ").unwrap();
        let state = load_from(&path);
        assert_eq!(state, WidgetState::default());
        assert!(!state.visible);
        assert_eq!(parse_state("not json at all"), WidgetState::default());
        assert_eq!(parse_state("[1,2,3]"), WidgetState::default());
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = temp_file("missing");
        assert_eq!(load_from(&path), WidgetState::default());
    }

    #[test]
    fn partial_and_bogus_values_are_normalised() {
        let s = parse_state(r#"{"visible":true,"x":10,"width":-5,"height":99999,"hotkey":"  "}"#);
        assert!(s.visible);
        assert_eq!((s.x, s.y), (None, None), "a lone x is not a position");
        assert_eq!(s.width, DEFAULT_WIDTH);
        assert_eq!(s.height, MAX_HEIGHT);
        assert!(s.hotkey.is_none());
        assert!(s.always_on_top, "missing fields keep their defaults");
    }

    #[test]
    fn patch_only_touches_given_fields() {
        let mut s = WidgetState::default();
        WidgetPatch {
            locked: Some(true),
            ..Default::default()
        }
        .apply(&mut s);
        assert!(s.locked);
        assert!(s.always_on_top);
        assert!(!s.click_through);
    }

    const PRIMARY: Rect = Rect::new(0, 0, 2560, 1400);
    const LEFT: Rect = Rect::new(-1920, 0, 1920, 1040);

    #[test]
    fn position_on_a_disconnected_monitor_moves_to_primary() {
        // Saved on a left-hand monitor (negative x) that is no longer there.
        let pos = clamp_to_monitors(Some((-1500, 200)), (300, 200), &[PRIMARY], 0, 24);
        assert_eq!(pos, (2560 - 300 - 24, 24));
    }

    #[test]
    fn position_on_a_negative_monitor_is_kept_when_it_exists() {
        let monitors = [PRIMARY, LEFT];
        let pos = clamp_to_monitors(Some((-1500, 200)), (300, 200), &monitors, 0, 24);
        assert_eq!(pos, (-1500, 200));
    }

    #[test]
    fn partly_offscreen_position_is_pulled_inside_its_monitor() {
        // Grip still visible on the primary, but the widget hangs off the right.
        let pos = clamp_to_monitors(Some((2400, 1300)), (300, 200), &[PRIMARY], 0, 24);
        assert_eq!(pos, (2560 - 300, 1400 - 200));
        // Hanging off the left edge of the left monitor.
        let pos = clamp_to_monitors(Some((-1950, 10)), (300, 200), &[PRIMARY, LEFT], 0, 24);
        assert_eq!(pos, (-1920, 10));
    }

    #[test]
    fn grip_only_barely_visible_is_parked() {
        // Only 10 px of the grip overlap the primary: not grabbable, re-park.
        let pos = clamp_to_monitors(Some((2550, 100)), (300, 200), &[PRIMARY], 0, 24);
        assert_eq!(pos, (2560 - 300 - 24, 24));
        // Above every monitor.
        let pos = clamp_to_monitors(Some((100, -500)), (300, 200), &[PRIMARY], 0, 24);
        assert_eq!(pos, (2560 - 300 - 24, 24));
    }

    #[test]
    fn unplaced_widget_parks_on_the_primary_even_when_it_is_not_first() {
        let pos = clamp_to_monitors(None, (300, 200), &[LEFT, PRIMARY], 1, 36);
        assert_eq!(pos, (2560 - 300 - 36, 36));
    }

    #[test]
    fn no_monitor_information_still_yields_a_sane_spot() {
        let pos = clamp_to_monitors(Some((99999, 99999)), (300, 200), &[], 0, 24);
        assert_eq!(pos, (1280 - 300 - 24, 24));
    }

    #[test]
    fn hotkeys_parse_into_win32_codes() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+W"),
            Ok((MOD_CONTROL | MOD_ALT, u32::from(b'W')))
        );
        assert_eq!(
            parse_hotkey("shift + win + 7"),
            Ok((MOD_SHIFT | MOD_WIN, u32::from(b'7')))
        );
        assert_eq!(parse_hotkey("Ctrl+F12"), Ok((MOD_CONTROL, 0x7B)));
        assert!(parse_hotkey("W").is_err(), "a bare key is refused");
        assert!(parse_hotkey("Ctrl+").is_err());
        assert!(parse_hotkey("Ctrl+Alt").is_err());
        assert!(parse_hotkey("Ctrl+A+B").is_err());
        assert!(parse_hotkey("Ctrl+F25").is_err());
        assert!(parse_hotkey("Ctrl+Enter").is_err());
    }
}
