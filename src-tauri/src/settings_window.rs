//! The settings window: a second webview served from its own URI scheme.
//!
//! The popover assets (`ui/**`) belong to the UI worker and are embedded at
//! compile time, so the settings window does not depend on a file existing
//! there. Instead the page is served by a Tauri URI-scheme handler
//! (`codexbar-settings://localhost/settings` → `http://codexbar-settings.localhost/settings`
//! on Windows). Tauri treats that origin as local, so `window.__TAURI__.core.invoke`
//! works exactly like it does in the popover.
//!
//! The page is deliberately self-contained: inline CSS/JS, no assets, no
//! network. It is the same dark palette as `ui/styles.css`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::webview::PageLoadEvent;
use tauri::window::Color;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// Window label of the settings window.
pub const SETTINGS_WINDOW: &str = "settings";
/// URI scheme registered for the settings page.
pub const SETTINGS_SCHEME: &str = "codexbar-settings";

/// Size of the settings window when it is first created.
const WIDTH: f64 = 560.0;
const HEIGHT: f64 = 880.0;

/// The page background (`--bg` in `ui/styles.css`), handed to both the window
/// and the webview at creation.
///
/// Windows composites the window rectangle as soon as it is visible, which is
/// *before* WebView2 has painted anything. Without this the first frames of
/// every settings/popover open were the shell's default light surface (the
/// reported "white flash"); with it they are the theme colour.
pub const BACKGROUND: Color = Color(0x0d, 0x0f, 0x13, 0xff);

/// How long to wait for the page-load event before showing the window anyway.
///
/// The page-load callback is the normal path; this only fires if it never
/// arrives (a wedged webview), so the user can never end up with a window that
/// refuses to appear.
const SHOW_FALLBACK_MS: u64 = 2500;

/// True once the settings page has reported `PageLoadEvent::Finished`.
static READY: AtomicBool = AtomicBool::new(false);

/// Set when something asked for the window while it was still hidden: the
/// page-load callback consumes it to show the window *after* the page can
/// paint.
static SHOW_WHEN_READY: AtomicBool = AtomicBool::new(false);

/// Guards against piling up fallback timers for repeated `open()` calls.
static FALLBACK_ARMED: AtomicBool = AtomicBool::new(false);

/// URL the settings webview is pointed at.
///
/// `builtin` forces the self-contained fallback page even when the UI worker's
/// `ui/settings.html` is embedded. It exists for evidence (`--settings-builtin`)
/// and for diagnosing a broken frontend bundle.
fn settings_url(builtin: bool) -> tauri::Url {
    let query = if builtin { "?builtin=1" } else { "" };
    format!("{SETTINGS_SCHEME}://localhost/settings{query}")
        .parse()
        .expect("settings url is a valid literal")
}

/// Serve the settings page.
///
/// The UI worker owns `ui/settings.html` (+ its `settings.js` / `styles.css`):
/// when that file is part of the embedded frontend it wins, and every relative
/// asset it asks for is proxied from the same embedded bundle so the page
/// renders exactly as it does over the app protocol. When `ui/settings.html` is
/// not embedded, the self-contained fallback page below is served instead, so
/// the settings window always works.
pub fn protocol_response(
    ctx: tauri::UriSchemeContext<'_, tauri::Wry>,
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    let requested = request.uri().path().trim_start_matches('/').to_string();
    let wants_builtin = request
        .uri()
        .query()
        .is_some_and(|query| query.split('&').any(|pair| pair == "builtin=1"));
    let resolver = ctx.app_handle().asset_resolver();

    // `/settings` (and the bare origin) is the entry point: prefer the UI
    // worker's page, fall back to the built-in one. `?builtin=1` forces the
    // built-in page.
    let (asset_name, is_entry) = match requested.as_str() {
        "" | "settings" | "settings/" | "index.html" => ("settings.html".to_string(), true),
        other => (other.to_string(), false),
    };

    if is_entry && wants_builtin {
        return builtin_response();
    }

    match resolver.get(asset_name) {
        Some(asset) => tauri::http::Response::builder()
            .status(tauri::http::StatusCode::OK)
            .header(
                tauri::http::header::CONTENT_TYPE,
                asset.mime_type().to_string(),
            )
            .header(tauri::http::header::CACHE_CONTROL, "no-store")
            .header("X-CodexBar-Page", if is_entry { "ui" } else { "asset" })
            .body(asset.bytes().to_vec())
            .expect("static response is valid"),
        None if is_entry => builtin_response(),
        None => tauri::http::Response::builder()
            .status(tauri::http::StatusCode::NOT_FOUND)
            .header(
                tauri::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )
            .body(format!("not found: {requested}").into_bytes())
            .expect("static response is valid"),
    }
}

/// The self-contained settings page, served when the UI bundle is absent or
/// `?builtin=1` was requested.
fn builtin_response() -> tauri::http::Response<Vec<u8>> {
    tauri::http::Response::builder()
        .status(tauri::http::StatusCode::OK)
        .header(
            tauri::http::header::CONTENT_TYPE,
            "text/html; charset=utf-8",
        )
        .header(tauri::http::header::CACHE_CONTROL, "no-store")
        .header("X-CodexBar-Page", "builtin")
        .body(HTML.as_bytes().to_vec())
        .expect("static response is valid")
}

/// Show the settings window, creating it hidden on first use.
pub fn open(app: &AppHandle) {
    open_with(app, false)
}

/// Show the settings window forced to the self-contained built-in page.
///
/// Used by `--settings-builtin` for evidence: it exercises the real window, the
/// real Tauri bridge and the real Rust commands, but never depends on the state
/// of the UI worker's `ui/settings.html`.
pub fn open_builtin(app: &AppHandle) {
    open_with(app, true)
}

/// Create the settings window hidden so the first open reuses a page that has
/// already been loaded and painted.
///
/// Nothing is shown: the window only becomes visible when [`open`] (or the
/// tray menu / `open_settings` command) asks for it. Reusing one window is also
/// what keeps the memory footprint flat — a hide/show cycle, never a rebuild.
pub fn prewarm(app: &AppHandle) {
    if app.get_webview_window(SETTINGS_WINDOW).is_some() {
        return;
    }
    READY.store(false, Ordering::SeqCst);
    if let Err(err) = build_hidden(app, false) {
        // Not fatal: `open` still creates the window on demand.
        eprintln!("codexbar: could not pre-warm the settings window: {err}");
    }
}

/// Build the settings webview hidden, with the dark background and the
/// page-load hook that shows it once it can paint.
fn build_hidden(app: &AppHandle, builtin: bool) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(
        app,
        SETTINGS_WINDOW,
        WebviewUrl::CustomProtocol(settings_url(builtin)),
    )
    .title("CodexBar Settings")
    .inner_size(WIDTH, HEIGHT)
    .min_inner_size(460.0, 420.0)
    .resizable(true)
    .center()
    // Dark window + webview surface: any frame composed before the page paints
    // is the theme colour, never the shell default (the reported white flash).
    .background_color(BACKGROUND)
    // Never visible at creation: Windows would paint an empty light rectangle
    // for the ~1-2 s WebView2 needs to compose the first frame.
    .visible(false)
    .on_page_load(|win, payload| {
        if matches!(payload.event(), PageLoadEvent::Finished) {
            READY.store(true, Ordering::SeqCst);
            if SHOW_WHEN_READY.swap(false, Ordering::SeqCst) {
                let _ = win.show();
                let _ = win.set_focus();
            }
        }
    })
    .build()
}

/// Show the window if the page is ready, otherwise remember the request and
/// show it as soon as the page has loaded (with a timer as the safety net).
fn open_with(app: &AppHandle, builtin: bool) {
    if let Some(win) = app.get_webview_window(SETTINGS_WINDOW) {
        if READY.load(Ordering::SeqCst) {
            // Pre-warmed: the page is already there, so this is a plain show.
            let _ = win.show();
            let _ = win.unminimize();
            let _ = win.set_focus();
        } else {
            SHOW_WHEN_READY.store(true, Ordering::SeqCst);
            arm_show_fallback(app);
        }
        return;
    }

    READY.store(false, Ordering::SeqCst);
    SHOW_WHEN_READY.store(true, Ordering::SeqCst);
    match build_hidden(app, builtin) {
        // Focus is applied by the page-load callback (or the fallback): showing
        // now would paint the not-yet-composed rectangle.
        Ok(_win) => arm_show_fallback(app),
        Err(err) => eprintln!("codexbar: could not open the settings window: {err}"),
    }
}

/// Show the settings window if the page-load event never arrived.
///
/// Runs on its own thread so it can never block the app; a single timer is
/// armed at a time, so repeated `open()` calls cannot pile them up.
fn arm_show_fallback(app: &AppHandle) {
    if FALLBACK_ARMED.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(SHOW_FALLBACK_MS));
        FALLBACK_ARMED.store(false, Ordering::SeqCst);
        if !SHOW_WHEN_READY.swap(false, Ordering::SeqCst) {
            return; // the page-load callback already showed it
        }
        if let Some(win) = handle.get_webview_window(SETTINGS_WINDOW) {
            eprintln!("codexbar: settings page did not report a load; showing it anyway");
            let _ = win.show();
            let _ = win.set_focus();
        }
    });
}

/// Hide (not destroy) the settings window so reopening is instant.
pub fn hide(app: &AppHandle) {
    if let Some(win) = app.get_webview_window(SETTINGS_WINDOW) {
        let _ = win.hide();
    }
}

/// The page itself.
pub const HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>CodexBar Settings</title>
<style>
  :root {
    --bg: #16181d; --panel: #1e2128; --panel-2: #23262e; --line: #32363f;
    --text: #f5f7fa; --muted: #98a0ad; --accent: #4c8dff; --healthy: #22c55e;
    --warning: #f59e0b; --critical: #ef4444;
  }
  * { box-sizing: border-box; }
  html, body { margin: 0; height: 100%; }
  body {
    background: var(--bg); color: var(--text); font: 13px/1.45 "Segoe UI", system-ui, sans-serif;
    padding: 0 0 12px;
  }
  header {
    display: flex; align-items: center; gap: 10px;
    padding: 14px 18px; border-bottom: 1px solid var(--line);
    background: var(--panel); position: sticky; top: 0; z-index: 2;
  }
  header .mark {
    width: 18px; height: 18px; border-radius: 50%;
    background: conic-gradient(var(--healthy) 0turn .42turn, #3a3f4a .42turn 1turn);
  }
  header h1 { font-size: 14px; margin: 0; font-weight: 600; }
  header .spacer { flex: 1; }
  section { margin: 14px 18px 0; background: var(--panel); border: 1px solid var(--line); border-radius: 10px; padding: 12px 14px; }
  section h2 { font-size: 12px; text-transform: uppercase; letter-spacing: .06em; color: var(--muted); margin: 0 0 8px; font-weight: 600; }
  p.hint { color: var(--muted); margin: 0 0 10px; font-size: 12px; }
  ul { list-style: none; margin: 0; padding: 0; }
  ul#providers { max-height: 176px; overflow-y: auto; }
  li.provider {
    display: flex; align-items: center; gap: 10px;
    padding: 4px 8px; border-radius: 8px; background: var(--panel-2); margin-bottom: 4px;
  }
  li.provider .name { flex: 1; }
  li.provider .id { color: var(--muted); font-size: 11px; font-family: Consolas, monospace; }
  li.provider button { background: #2c313a; color: var(--text); border: 1px solid var(--line); border-radius: 6px; width: 26px; height: 24px; cursor: pointer; }
  li.provider button:disabled { opacity: .35; cursor: default; }
  ul#sessions li.session {
    display: flex; align-items: center; gap: 10px;
    padding: 6px 8px; border-radius: 8px; background: var(--panel-2); margin-bottom: 4px;
  }
  li.session .name { flex: 1; }
  li.session .id { color: var(--muted); font-size: 11px; font-family: Consolas, monospace; }
  li.session .state { font-size: 11px; font-family: Consolas, monospace; color: var(--muted); }
  li.session .state.expired { color: var(--critical); }
  li.session .state.ok { color: var(--healthy); }
  label.row { display: flex; align-items: center; gap: 8px; padding: 5px 0; }
  label.row input[type=number] {
    width: 96px; background: var(--panel-2); color: var(--text);
    border: 1px solid var(--line); border-radius: 6px; padding: 5px 8px;
  }
  input[type=checkbox] { width: 15px; height: 15px; accent-color: var(--accent); }
  footer { display: flex; gap: 8px; align-items: center; margin: 16px 18px 0; }
  footer .spacer { flex: 1; }
  button.act {
    background: #2c313a; color: var(--text); border: 1px solid var(--line);
    border-radius: 8px; padding: 7px 12px; cursor: pointer; font-size: 13px;
  }
  button.act.primary { background: var(--accent); border-color: var(--accent); color: #08101f; font-weight: 600; }
  button.act:hover { filter: brightness(1.12); }
  #status { color: var(--muted); font-size: 12px; }
  #status.ok { color: var(--healthy); }
  #status.err { color: var(--critical); }
  .meta { margin: 12px 18px 0; color: var(--muted); font-size: 11px; font-family: Consolas, monospace; word-break: break-all; }
  .meta b { color: var(--text); font-weight: 600; }
</style>
</head>
<body>
<header>
  <span class="mark"></span>
  <h1>CodexBar Settings</h1>
  <span class="spacer"></span>
  <button class="act" id="btnClose" title="Close (Esc)">Close</button>
</header>

<section>
  <h2>Providers</h2>
  <p class="hint">Enabled providers get their own tray icon unless icons are merged. The order here is the tray and menu order.</p>
  <ul id="providers"></ul>
</section>

<section>
  <h2>Sessions</h2>
  <p class="hint">CodexBar for Windows does <b>not</b> refresh tokens: a dead session is delegated to the CLI that owns it, exactly like the original app. Re-authenticate launches <code>codex login</code> / <code>claude</code> in its own window — the CLI writes its own credentials, CodexBar never sees them.</p>
  <ul id="sessions"></ul>
  <div class="meta" id="reauthState"></div>
</section>

<section>
  <h2>Sign-in</h2>
  <p class="hint">GitHub Copilot uses a device code: start it here, type the code on the GitHub page, then poll. The issued token is written to the port's config (atomically); it is never printed.</p>
  <div class="row" style="display:flex;gap:8px;align-items:center;flex-wrap:wrap">
    <button class="act" id="btnCopilot">Sign in with GitHub (Copilot)</button>
    <button class="act" id="btnCopilotPoll" disabled>Check</button>
    <button class="act" id="btnCopilotCancel" disabled>Cancel</button>
  </div>
  <div class="meta" id="loginState"></div>
</section>

<section>
  <h2>Refresh</h2>
  <label class="row">Every <input id="interval" type="number" min="15" max="86400" step="15" aria-label="Refresh interval in seconds" /> seconds</label>
  <label class="row"><input id="refreshCredentials" type="checkbox" aria-label="Refresh stored credentials during a refresh" /> Refresh stored credentials during a refresh</label>
</section>

<section>
  <h2>Data</h2>
  <p class="hint">Live is the default: this machine's local credentials are read and real usage is fetched, the same registry the CLI's <code>--live</code> uses. Sample data calls nothing and reads nothing.</p>
  <label class="row"><input id="mockMode" type="checkbox" aria-label="Serve sample data instead of live usage" /> Serve sample data (no credentials read, no network calls)</label>
  <div class="meta" id="modeInfo"></div>
</section>

<section>
  <h2>Tray</h2>
  <label class="row"><input id="mergeIcons" type="checkbox" aria-label="Merge into a single tray icon (with a provider selector)" /> Merge into a single tray icon (with a provider selector)</label>
  <label class="row"><input id="showPercentInIcon" type="checkbox" aria-label="Draw the used percentage into the icon" /> Draw the used percentage into the icon</label>
</section>

<section>
  <h2>Windows</h2>
  <label class="row"><input id="startAtLogin" type="checkbox" aria-label="Start CodexBar when I sign in" /> Start CodexBar when I sign in</label>
  <div class="meta" id="autostart"></div>
</section>

<footer>
  <button class="act primary" id="btnSave">Save</button>
  <button class="act" id="btnDefaults">Restore defaults</button>
  <span class="spacer"></span>
  <span id="status"></span>
  <button class="act" id="btnRefresh">Refresh now</button>
  <button class="act" id="btnQuit">Quit CodexBar</button>
</footer>

<div class="meta" id="paths"></div>

<script>
(function () {
  "use strict";
  var T = window.__TAURI__ || {};
  var invoke = T.core && T.core.invoke;
  if (!invoke) {
    document.getElementById("status").textContent = "Tauri bridge unavailable";
    document.getElementById("status").className = "err";
    return;
  }

  var state = { settings: null, catalog: [], info: null };

  function el(id) { return document.getElementById(id); }

  function setStatus(text, kind) {
    var s = el("status");
    s.textContent = text || "";
    s.className = kind || "";
  }

  function titleFor(id) {
    for (var i = 0; i < state.catalog.length; i++) {
      if (state.catalog[i].id === id) return state.catalog[i].title;
    }
    return id;
  }

  function renderProviders() {
    var list = el("providers");
    list.textContent = "";
    state.settings.providers.forEach(function (pref, index) {
      var li = document.createElement("li");
      li.className = "provider";

      var box = document.createElement("input");
      box.type = "checkbox";
      box.checked = !!pref.enabled;
      box.dataset.id = pref.id;
      box.setAttribute("aria-label", "Enable " + titleFor(pref.id));
      box.addEventListener("change", function () { pref.enabled = box.checked; });
      li.appendChild(box);

      var name = document.createElement("span");
      name.className = "name";
      name.textContent = titleFor(pref.id);
      var id = document.createElement("span");
      id.className = "id";
      id.textContent = " " + pref.id;
      name.appendChild(id);
      li.appendChild(name);

      var up = document.createElement("button");
      up.textContent = "↑";
      up.disabled = index === 0;
      up.title = "Move up";
      up.setAttribute("aria-label", "Move " + titleFor(pref.id) + " up");
      up.addEventListener("click", function () { move(index, -1); });
      li.appendChild(up);

      var down = document.createElement("button");
      down.textContent = "↓";
      down.disabled = index === state.settings.providers.length - 1;
      down.title = "Move down";
      down.setAttribute("aria-label", "Move " + titleFor(pref.id) + " down");
      down.addEventListener("click", function () { move(index, 1); });
      li.appendChild(down);

      list.appendChild(li);
    });
  }

  function move(index, delta) {
    var target = index + delta;
    var list = state.settings.providers;
    if (target < 0 || target >= list.length) return;
    var tmp = list[index];
    list[index] = list[target];
    list[target] = tmp;
    renderProviders();
  }

  function renderForm() {
    el("interval").value = state.settings.refreshIntervalSecs;
    el("refreshCredentials").checked = !!state.settings.refreshCredentials;
    el("mockMode").checked = !!state.settings.mockMode;
    el("mergeIcons").checked = !!state.settings.mergeIcons;
    el("showPercentInIcon").checked = !!state.settings.showPercentInIcon;
    el("startAtLogin").checked = !!state.settings.startAtLogin;
    renderProviders();

    if (state.info) {
      el("paths").innerHTML =
        "<b>config</b> " + state.info.configPath +
        "<br><b>exe</b> " + state.info.exePath;
      el("autostart").textContent = state.info.autostartEnabled
        ? "Registered: " + (state.info.autostartCommand || "(value present)")
        : "Not registered (no HKCU\\...\\Run entry named CodexBar).";
      el("modeInfo").textContent = state.settings.mockMode
        ? "Mode: mock — every card is sample data (source: mock)."
        : "Mode: " + (state.info.mode || "live") +
          " — providers report their own status (ok / notConfigured / error).";
    }
  }

  function collect() {
    state.settings.refreshIntervalSecs = parseInt(el("interval").value, 10) || state.settings.refreshIntervalSecs;
    state.settings.refreshCredentials = el("refreshCredentials").checked;
    state.settings.mockMode = el("mockMode").checked;
    state.settings.mergeIcons = el("mergeIcons").checked;
    state.settings.showPercentInIcon = el("showPercentInIcon").checked;
    state.settings.startAtLogin = el("startAtLogin").checked;
    return state.settings;
  }

  async function load() {
    var pair = await Promise.all([invoke("get_settings"), invoke("settings_info")]);
    state.settings = pair[0];
    state.info = pair[1];
    state.catalog = pair[1].catalog || [];
    renderForm();
    setStatus("Loaded.", "ok");
    loadSessions().catch(function () {});
  }

  async function save() {
    setStatus("Saving…");
    try {
      var saved = await invoke("set_settings", { settings: collect() });
      state.settings = saved;
      state.info = await invoke("settings_info");
      renderForm();
      setStatus("Saved to " + (state.info && state.info.configPath ? state.info.configPath : "disk"), "ok");
    } catch (err) {
      setStatus("Save failed: " + err, "err");
    }
  }

  el("btnSave").addEventListener("click", save);
  el("btnDefaults").addEventListener("click", async function () {
    var defaults = await invoke("default_settings");
    state.settings = defaults;
    renderForm();
    setStatus("Defaults loaded — press Save to persist.", "");
  });
  el("btnRefresh").addEventListener("click", async function () {
    setStatus("Refreshing providers…");
    try {
      var report = await invoke("refresh_now");
      setStatus("Refreshed " + (report.providers ? report.providers.length : 0) + " providers.", "ok");
      loadSessions().catch(function () {});
    } catch (err) {
      setStatus("Refresh failed: " + err, "err");
    }
  });
  el("btnQuit").addEventListener("click", function () { invoke("quit"); });
  el("btnClose").addEventListener("click", function () { invoke("hide_settings"); });

  /* ---- Copilot device flow -------------------------------------------------
   * start -> show userCode + open the verification page -> poll until GitHub
   * answers. `copilot_login_start` returns {userCode, verificationUri, …};
   * the device code itself stays in the Rust process. A successful
   * authorization reports `stored: false` today (the provider crate has no
   * token store yet), so the note says so instead of claiming success. */
  function loginNote(text) { el("loginState").textContent = text || ""; }

  el("btnCopilot").addEventListener("click", async function () {
    loginNote("Asking GitHub for a device code…");
    try {
      var started = await invoke("copilot_login_start");
      loginNote("Code " + started.userCode + " — open " + started.verificationUri +
        " and approve the sign-in. " + started.message);
      el("btnCopilotPoll").disabled = false;
      el("btnCopilotCancel").disabled = false;
    } catch (err) {
      loginNote("Could not start the sign-in: " + err);
    }
  });

  el("btnCopilotPoll").addEventListener("click", async function () {
    try {
      var poll = await invoke("copilot_login_poll");
      loginNote(poll.status + ": " + poll.message +
        (poll.extensionPoint ? " (extension point: " + poll.extensionPoint + ")" : ""));
      if (poll.status !== "pending" && poll.status !== "slowDown") {
        el("btnCopilotPoll").disabled = true;
        el("btnCopilotCancel").disabled = true;
      }
    } catch (err) {
      loginNote("Could not check the sign-in: " + err);
    }
  });

  el("btnCopilotCancel").addEventListener("click", async function () {
    await invoke("copilot_login_cancel");
    el("btnCopilotPoll").disabled = true;
    el("btnCopilotCancel").disabled = true;
    loginNote("Sign-in cancelled — nothing was stored.");
  });

  /* ---- Sessions / re-authentication ---------------------------------------
   * `session_states` returns { sessions: [...], reauth: {...} }. A dead session
   * on a provider whose CLI this port knows (`codex login`, `claude`) gets a
   * Re-authenticate button. Clicking it launches the CLI in its own console;
   * this page only polls the launch state — it never sees a credential. */
  var reauthTimer = null;

  function reauthNote(text, kind) {
    var box = el("reauthState");
    box.textContent = text || "";
    box.className = "meta" + (kind ? " " + kind : "");
  }

  function pollReauth() {
    if (reauthTimer) return;
    reauthTimer = setInterval(function () {
      invoke("reauth_status").then(function (next) {
        reauthNote(next.state + ": " + next.message + (next.pid ? " [pid " + next.pid + "]" : ""));
        if (next.state !== "running" && next.state !== "launched") {
          clearInterval(reauthTimer); reauthTimer = null;
          loadSessions();
        }
      }).catch(function () { clearInterval(reauthTimer); reauthTimer = null; });
    }, 1500);
  }

  function renderReauth(status) {
    if (!status) return;
    reauthNote(status.state + ": " + status.message + (status.pid ? " [pid " + status.pid + "]" : ""));
    if (status.state === "running" || status.state === "launched") pollReauth();
  }

  function loadSessions() {
    return invoke("session_states").then(function (payload) {
      var list = el("sessions");
      list.textContent = "";
      (payload.sessions || []).forEach(function (session) {
        if (!session.canReauth && !session.tokenExpired) return;
        var li = document.createElement("li");
        li.className = "session";

        var name = document.createElement("span");
        name.className = "name";
        name.textContent = session.title + " ";
        var id = document.createElement("span");
        id.className = "id";
        id.textContent = session.provider;
        name.appendChild(id);
        li.appendChild(name);

        var state = document.createElement("span");
        state.className = "state " + (session.tokenExpired ? "expired" : "");
        state.textContent = session.tokenExpired
          ? "token expired — session dead"
          : (session.sessionState === "error" ? "error — may need re-auth" : (session.status || session.sessionState));
        li.appendChild(state);

        if (session.canReauth) {
          var btn = document.createElement("button");
          btn.className = "act";
          btn.textContent = "Re-authenticate";
          btn.title = session.reauthHint || "";
          btn.setAttribute("aria-label", "Re-authenticate " + session.title);
          btn.addEventListener("click", function () {
            reauthNote("Launching " + (session.reauthCommand || "the sign-in") + "…");
            invoke("reauth_launch", { provider: session.provider })
              .then(renderReauth)
              .catch(function (err) { reauthNote("Could not launch: " + err, "err"); });
          });
          li.appendChild(btn);
        }
        list.appendChild(li);
      });
      if (!list.children.length) {
        var empty = document.createElement("li");
        empty.className = "session";
        empty.textContent = "No session needs re-authentication right now.";
        list.appendChild(empty);
      }
      renderReauth(payload.reauth);
    });
  }

  document.addEventListener("keydown", function (event) {
    if (event.key === "Escape") invoke("hide_settings");
  });

  load().catch(function (err) { setStatus("Could not load settings: " + err, "err"); });
})();
</script>
</body>
</html>
"#;
