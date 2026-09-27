/* CodexBar for Windows — floating desktop widget.
 *
 * One compact row per *active* provider (enabled in Settings and set up):
 * icon, name, usage bar, headline %, and time until that lane resets.
 *
 * Data: the same `UsageReport` the popover renders — `invoke("get_report")`
 * once, then every `usage-updated` event the refresh tick emits. The widget
 * never talks to a provider itself. Outside Tauri (static renders) it falls
 * back to `window.__CODEXBAR_MOCK__` from mock.js, with timestamps rebased to
 * "now" so the countdowns read naturally.
 *
 * Staleness: a row whose snapshot says `stale` gets a marker, and the header
 * shows "Updated 12m ago" once the whole report is older than twice the
 * refresh interval (or than 10 minutes when the interval is unknown).
 *
 * Preview-only query parameters (ignored inside Tauri):
 *   ?theme=light|dark   pin the fallback palette
 *   ?stale=1            age the mock report to show the stale banner
 */
/**
 * Compact countdown for the narrow reset column: at most two units, and a
 * unit that is zero is dropped ("3d", not "3d 0h"; "2h", not "2h 0m").
 * Pure and DOM-free so `node --test ui/widget.test.js` can exercise it.
 */
function widgetDuration(ms) {
  const secs = Math.max(0, Math.round(Number(ms) / 1000) || 0);
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  const minutes = Math.floor((secs % 3600) / 60);
  const join = (a, ua, b, ub) => (b > 0 ? a + ua + " " + b + ub : a + ua);
  if (days > 0) return join(days, "d", hours, "h");
  if (hours > 0) return join(hours, "h", minutes, "m");
  if (minutes > 0) return minutes + "m";
  return secs + "s";
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = { widgetDuration };
}

if (typeof document !== "undefined") (function () {
  "use strict";

  const TAURI = window.__TAURI__ || null;
  const invoke = TAURI && TAURI.core ? TAURI.core.invoke : null;
  const listen = TAURI && TAURI.event ? TAURI.event.listen : null;
  const F = window.CodexBarFormat;
  const params = new URLSearchParams(location.search);

  const els = {
    root: document.getElementById("widget"),
    grip: document.getElementById("grip"),
    rows: document.getElementById("rows"),
    empty: document.getElementById("empty"),
    stale: document.getElementById("stale"),
    pin: document.getElementById("btn-pin"),
    lock: document.getElementById("btn-lock"),
    hide: document.getElementById("btn-hide"),
  };

  let report = null;
  let settings = null;
  let widgetState = { alwaysOnTop: true, locked: false, clickThrough: false };

  /* ---------- theme (optional, from ui/theme.js) ----------------------- */

  function applyTheme() {
    const pinned = !invoke && params.get("theme");
    if (pinned === "light" || pinned === "dark") {
      document.documentElement.setAttribute("data-theme", pinned);
    }
    const theme = window.CodexBarTheme;
    if (!theme) return;
    try {
      if (typeof theme.apply === "function") theme.apply(document.documentElement);
      if (typeof theme.subscribe === "function") {
        theme.subscribe(() => theme.apply && theme.apply(document.documentElement));
      }
    } catch (err) {
      // A broken theme must never blank the widget: the CSS fallbacks stay.
      console.warn("CodexBar widget: theme.js failed, using fallback palette", err);
    }
  }

  /* ---------- helpers ---------------------------------------------------- */

  async function call(name, args) {
    if (!invoke) return null;
    try {
      return await invoke(name, args);
    } catch (err) {
      console.warn("CodexBar widget: " + name + " failed", err);
      return null;
    }
  }

  function enabledIds() {
    if (!settings || !Array.isArray(settings.providers)) return null;
    return new Set(settings.providers.filter((p) => p.enabled !== false).map((p) => p.id));
  }

  /** Enabled + configured providers, in report (canonical) order. */
  function activeProviders(rep) {
    const enabled = enabledIds();
    return ((rep && rep.providers) || []).filter(
      (p) => p.status !== "notConfigured" && (!enabled || enabled.has(p.provider))
    );
  }

  function staleThresholdMs() {
    const secs = settings && Number(settings.refreshIntervalSecs);
    return secs > 0 ? Math.max(2 * secs * 1000, 60_000) : 10 * 60_000;
  }

  function isReportStale(rep, now) {
    const t = Date.parse(rep && rep.generatedAt);
    return Number.isFinite(t) && now - t > staleThresholdMs();
  }

  function shortReset(win, now) {
    if (!win) return "";
    if (win.resetsAt) {
      const left = Date.parse(win.resetsAt) - now;
      if (left > 0) return widgetDuration(left);
      return "due";
    }
    return "";
  }

  /** Provider mark on a uniform light chip (see widget.css `.widget-chip`):
   *  the SVGs keep their own colours, so full-bleed tiles (codex, zai) and
   *  near-black marks (cursor, copilot) all read in both themes. */
  function iconFor(snapshot) {
    const chip = document.createElement("span");
    chip.className = "widget-chip";
    const img = document.createElement("img");
    img.className = "widget-icon";
    img.alt = "";
    img.src = "assets/" + snapshot.provider + ".svg";
    img.addEventListener("error", () => {
      chip.classList.add("is-fallback");
      chip.textContent = (snapshot.title || snapshot.provider || "?").charAt(0).toUpperCase();
    });
    chip.appendChild(img);
    return chip;
  }

  function cell(cls, text) {
    const el = document.createElement("span");
    el.className = cls;
    if (text != null) el.textContent = text;
    return el;
  }

  /* ---------- render ----------------------------------------------------- */

  function renderRow(snapshot, now) {
    const li = document.createElement("li");
    li.className = "widget-row";
    li.appendChild(iconFor(snapshot));
    li.appendChild(cell("widget-name", snapshot.title || snapshot.provider));

    const lane = F.headlineWindow(snapshot);
    const state = F.classify(snapshot);
    const used = lane ? F.clampPct(lane.window.usedPercent) : null;

    if (used == null) {
      li.classList.add("is-muted");
      const note =
        snapshot.status === "error" ? state.label || "Fetch failed" : state.label || "No usage";
      if (snapshot.status === "error") li.dataset.tone = "error";
      li.appendChild(cell("widget-note", note));
      li.title = (snapshot.title || "") + " — " + (snapshot.error || note);
      return li;
    }

    li.dataset.tone = F.severityFor(used);
    if (snapshot.status === "stale") li.classList.add("is-stale");
    if (snapshot.status === "error") li.classList.add("is-muted");

    const bar = cell("widget-bar");
    const fill = cell("widget-bar-fill");
    fill.style.width = used + "%";
    bar.appendChild(fill);
    li.appendChild(bar);
    li.appendChild(cell("widget-pct", Math.round(used) + "%"));
    li.appendChild(cell("widget-reset", shortReset(lane.window, now)));

    const parts = [
      snapshot.title + " · " + lane.title,
      F.pct(used) + " used",
      F.resetText(lane.window, now),
    ];
    if (snapshot.status === "stale") parts.push("stale data");
    li.title = parts.filter(Boolean).join(" — ");
    return li;
  }

  function render() {
    const now = Date.now();
    const active = activeProviders(report);
    els.rows.replaceChildren(...active.map((p) => renderRow(p, now)));
    els.empty.hidden = active.length > 0;

    const stale = report && isReportStale(report, now);
    els.stale.hidden = !stale;
    els.stale.textContent = stale ? "Updated " + F.ageLabel(report.generatedAt) : "";
    els.root.classList.toggle("is-stale", !!stale);
  }

  function renderState() {
    els.pin.setAttribute("aria-pressed", String(!!widgetState.alwaysOnTop));
    els.lock.setAttribute("aria-pressed", String(!!widgetState.locked));
    els.lock.title = widgetState.locked ? "Unlock position" : "Lock position";
    els.root.classList.toggle("is-locked", !!widgetState.locked);
  }

  /* ---------- mock (static renders) ------------------------------------- */

  /** Shift every timestamp so the mock report was generated "now". */
  function rebasedMock() {
    const mock = window.__CODEXBAR_MOCK__;
    if (!mock) return null;
    const base = Date.parse(mock.generatedAt);
    const age = params.get("stale") === "1" ? 47 * 60_000 : 0;
    const shift = Date.now() - base - age;
    const move = (iso) => (iso ? new Date(Date.parse(iso) + shift).toISOString() : iso);
    const copy = JSON.parse(JSON.stringify(mock));
    copy.generatedAt = move(copy.generatedAt);
    for (const p of copy.providers || []) {
      p.fetchedAt = move(p.fetchedAt);
      for (const w of p.windows || []) {
        if (w.window) w.window.resetsAt = move(w.window.resetsAt);
      }
    }
    return copy;
  }

  /* ---------- wiring ------------------------------------------------------ */

  function wireControls() {
    els.grip.addEventListener("mousedown", (e) => {
      if (e.button !== 0 || e.target.closest(".widget-btn")) return;
      if (widgetState.locked) return;
      call("widget_drag");
    });
    els.pin.addEventListener("click", async () => {
      const next = await call("widget_update", { patch: { alwaysOnTop: !widgetState.alwaysOnTop } });
      if (next) widgetState = next;
      renderState();
    });
    els.lock.addEventListener("click", async () => {
      const next = await call("widget_update", { patch: { locked: !widgetState.locked } });
      if (next) widgetState = next;
      renderState();
    });
    els.hide.addEventListener("click", () => call("widget_update", { patch: { visible: false } }));
  }

  async function start() {
    applyTheme();
    wireControls();

    if (!invoke) {
      document.body.classList.add("is-preview");
      report = rebasedMock();
      render();
      renderState();
      return;
    }

    const [rep, set, st] = await Promise.all([
      call("get_report"),
      call("get_settings"),
      call("widget_state"),
    ]);
    report = rep;
    settings = set;
    if (st) widgetState = st;
    render();
    renderState();

    if (listen) {
      try {
        await listen("usage-updated", async (event) => {
          report = event.payload;
          // `settings-updated` is only emitted to the popover and the settings
          // window, so re-read the (local, in-memory) settings with each report
          // to pick up providers toggled on/off. No network involved.
          const set = await call("get_settings");
          if (set) settings = set;
          render();
        });
        await listen("settings-updated", (event) => {
          settings = event.payload;
          render();
        });
        await listen("widget-state", (event) => {
          widgetState = event.payload || widgetState;
          renderState();
        });
      } catch (err) {
        console.warn("CodexBar widget: event listeners unavailable", err);
      }
    }
    // Countdowns and the stale banner move even between refreshes.
    setInterval(render, 30_000);
  }

  start();
})();
