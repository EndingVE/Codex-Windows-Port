/* CodexBar for Windows — settings window controller.
 *
 * Backend contract (src-tauri, `settings.rs` + `settings_window.rs`):
 *   invoke("get_settings")             -> Settings  (camelCase JSON)
 *   invoke("set_settings", {settings}) -> Result<Settings, String>
 *   invoke("default_settings")         -> Settings
 *   invoke("settings_info")            -> { configPath, configDir, exePath,
 *                                           autostartEnabled, autostartCommand,
 *                                           catalog: [{id, title, …}] }
 *   invoke("app_metadata")             -> { name, version, schemaVersion,
 *                                           mode, mockMode, … }
 *   invoke("get_report") / ("usage_snapshot") -> UsageReport
 *   invoke("session_states")           -> { sessions: [{provider, status,
 *                                           sessionState, tokenExpired, …}] }
 *   invoke("refresh_now") / ("quit") / ("hide_settings")
 *
 * The per-row status hints are read from `get_report` + `session_states` (the
 * same payloads the popover uses) whenever the backend is live. `ui/mock.js`
 * is only consulted with no backend at all, or when `app_metadata` reports
 * `mockMode` — so a live window can never print sample-data text.
 *
 * `Settings` fields (serde camelCase, `default`, unknown keys ignored):
 *   providers: [{ id, enabled }]   refreshIntervalSecs: u64
 *   startAtLogin: bool             mergeIcons: bool
 *   refreshCredentials: bool       showPercentInIcon: bool
 *   mergedProvider: string|null
 *
 * Nothing here requires Rust: with no bridge the same shape is kept in
 * localStorage and the provider set is the shipped one, so the window still
 * opens, renders and is screenshot-able in a plain browser.
 */
(function () {
  "use strict";

  const TAURI = window.__TAURI__ || null;
  const invoke = TAURI && TAURI.core ? TAURI.core.invoke : null;
  const F = window.CodexBarFormat;

  const STORAGE_KEY = "codexbar.settings.v1";

  /* Canonical provider set — mirrors `ProviderId::ALL` in codexbar-core
   * (14 entries, same display order). Titles match `ProviderId::title()`. */
  const PROVIDER_SET = [
    { id: "codex", title: "Codex", sub: "OpenAI Codex CLI + ChatGPT plans" },
    { id: "claude", title: "Claude", sub: "Claude Code OAuth / web session" },
    { id: "cursor", title: "Cursor", sub: "Cursor plan usage" },
    { id: "openrouter", title: "OpenRouter", sub: "Credit balance + spend" },
    { id: "copilot", title: "Copilot", sub: "GitHub Copilot seats" },
    { id: "gemini", title: "Gemini", sub: "Google AI Pro / API key" },
    { id: "deepseek", title: "DeepSeek", sub: "DeepSeek API key" },
    { id: "groq", title: "Groq", sub: "Groq API key" },
    { id: "zai", title: "z.ai", sub: "z.ai API key" },
    { id: "minimax", title: "MiniMax", sub: "MiniMax API key" },
    { id: "kimi", title: "Kimi", sub: "Kimi API key" },
    { id: "elevenlabs", title: "ElevenLabs", sub: "ElevenLabs API key" },
    { id: "xai", title: "xAI", sub: "xAI API key" },
    { id: "opencodego", title: "OpenCode Go", sub: "OpenCode Go API key" },
  ];

  /* Defaults mirror `Settings::default()` in src-tauri/src/settings.rs. */
  const DEFAULTS = {
    providers: PROVIDER_SET.map((p) => ({ id: p.id, enabled: true })),
    refreshIntervalSecs: 300,
    startAtLogin: false,
    mergeIcons: false,
    refreshCredentials: false,
    showPercentInIcon: true,
  };

  const els = {
    paneButtons: [...document.querySelectorAll(".nav-item[data-pane]")],
    panes: [...document.querySelectorAll(".pane")],
    toast: document.getElementById("toast"),
    sub: document.getElementById("settingsSub"),
    navCount: document.getElementById("navProvidersCount"),
    providerList: document.getElementById("providerList"),
    providerFilter: document.getElementById("providerFilter"),
    providerSummary: document.getElementById("providerSummary"),
    backend: document.getElementById("settingsBackend"),
    aboutName: document.getElementById("aboutName"),
    aboutVersion: document.getElementById("aboutVersion"),
    aboutSource: document.getElementById("aboutSource"),
    autostartInfo: document.getElementById("autostartInfo"),
    pathsInfo: document.getElementById("pathsInfo"),
  };

  let settings = null;
  let backend = "local"; // "tauri" | "local"
  let info = null;

  /* Per-provider state hints. In live mode these are read from the running
   * backend (`get_report` + `session_states`); the bundled sample fixture is
   * only consulted when there is no backend at all, or when the backend itself
   * reports that it is in mock mode. `statusMode` is what each row's
   * `live` / `sample` pill shows. */
  let statusMap = new Map();
  let statusMode = "sample";

  /* ------------------------------------------------------------------ */
  /* persistence                                                          */
  /* ------------------------------------------------------------------ */

  function normalize(raw) {
    const next = Object.assign({}, DEFAULTS, raw || {});
    const known = new Map(PROVIDER_SET.map((p) => [p.id, p]));
    const seen = new Set();
    const providers = [];
    (Array.isArray(next.providers) ? next.providers : []).forEach((entry) => {
      const id = typeof entry === "string" ? entry : entry && entry.id;
      if (!known.has(id) || seen.has(id)) return;
      seen.add(id);
      // Keep the entry's other fields (e.g. `{hasKey, masked}` placeholders the
      // backend sends instead of secrets). The backend merges every save with
      // what is on disk, so a field left out here never deletes a stored key.
      const rest = entry && typeof entry === "object" ? entry : {};
      providers.push(Object.assign({}, rest, { id, enabled: rest.enabled !== false }));
    });
    // Any provider the backend did not mention still shows up, so a new build
    // cannot hide its own providers behind an old settings file.
    PROVIDER_SET.forEach((p) => {
      if (!seen.has(p.id)) providers.push({ id: p.id, enabled: true });
    });
    next.providers = providers;
    next.refreshIntervalSecs = Number(next.refreshIntervalSecs) || DEFAULTS.refreshIntervalSecs;
    return next;
  }

  function localRead() {
    try {
      const raw = window.localStorage.getItem(STORAGE_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch (err) {
      return null;
    }
  }

  function localWrite() {
    try {
      window.localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
    } catch (err) {
      /* not fatal: the window keeps working with an in-memory copy */
    }
  }

  async function load() {
    if (invoke) {
      try {
        const remote = await invoke("get_settings");
        backend = "tauri";
        return normalize(remote);
      } catch (err) {
        console.warn("CodexBar: get_settings unavailable", err);
      }
    }
    return normalize(localRead());
  }

  /** Persist. With a backend this is authoritative; localStorage is always
   *  mirrored so a browser-only session still remembers what was set. */
  /** Secret fields never travel back as placeholders: a masked or empty
   *  value is dropped, which the backend reads as "unchanged". */
  const SECRET_FIELDS = ["apiKey", "cookieHeader", "token"];
  function withoutSecretPlaceholders(value) {
    const providers = (value.providers || []).map((entry) => {
      const out = Object.assign({}, entry);
      SECRET_FIELDS.forEach((field) => {
        const v = out[field];
        if (v === undefined || v === null) return;
        if (typeof v !== "string" || !v.trim() || v.startsWith("•")) delete out[field];
      });
      delete out.tokenAccounts;
      return out;
    });
    return Object.assign({}, value, { providers });
  }

  async function save(next) {
    settings = normalize(next);
    localWrite();
    if (invoke) {
      try {
        const echo = await invoke("set_settings", {
          settings: withoutSecretPlaceholders(settings),
        });
        if (echo) settings = normalize(echo);
      } catch (err) {
        console.warn("CodexBar: set_settings failed, kept the local copy", err);
        toast("Backend rejected the save — kept locally.");
      }
    }
  }

  /** Persist a change, then re-sync the painted controls against whatever the
   *  backend made authoritative (the `set_settings` echo, or the pushed
   *  `settings-updated` payload). The re-sync is by id and in place, so a row
   *  the user just clicked keeps its focus instead of being rebuilt under it. */
  function patch(changes) {
    return save(Object.assign({}, settings, changes)).then(() => {
      syncProviderRows();
      renderBackendLine();
    });
  }

  /** Adopt an authoritative payload pushed by the backend (`settings-updated`,
   *  emitted by `set_settings` and by the tray menu). Provider rows are updated
   *  in place — never rebuilt — so this cannot clobber a click that is already
   *  on its way, and controls do not flicker. */
  function adoptRemote(next) {
    if (!next) return;
    settings = normalize(next);
    localWrite();
    syncProviderRows();
    paintControls();
    renderBackendLine();
  }

  /* ------------------------------------------------------------------ */
  /* controls                                                             */
  /* ------------------------------------------------------------------ */

  function paintSwitch(id, on) {
    const node = document.getElementById(id);
    if (node) node.setAttribute("aria-checked", String(!!on));
  }

  function bindSwitch(id, apply, message) {
    const node = document.getElementById(id);
    if (!node) return;
    node.addEventListener("click", () => {
      const next = node.getAttribute("aria-checked") !== "true";
      node.setAttribute("aria-checked", String(next));
      apply(next);
      if (message) toast(message(next));
    });
  }

  /* ------------------------------------------------------------------ */
  /* provider list + per-provider state hints (live vs sample)            */
  /* ------------------------------------------------------------------ */

  /** Normalise one ProviderSnapshot (live or fixture — same shape) so the
   *  hint builder does not care where the numbers came from. */
  function snapshotEntry(p) {
    const state = F ? F.classify(p) : { code: p.status || "ok", label: p.status || "ok" };
    return {
      code: state.code,
      label: state.label,
      error: p.error || "",
      used: F ? F.headline(p) : null,
      source: p.source,
      tokenExpired: state.code === "tokenExpired",
      sessionState: null,
      canReauth: false,
    };
  }

  /** Map built from the bundled fixture — browser-only or backend mock mode. */
  function fixtureStatus() {
    const map = new Map();
    const report = window.__CODEXBAR_MOCK__;
    if (report && Array.isArray(report.providers)) {
      report.providers.forEach((p) => map.set(p.provider, snapshotEntry(p)));
    }
    return map;
  }

  /** Try each command name in order; the first that resolves wins. */
  async function invokeAny(names, args) {
    let last = null;
    for (const name of names) {
      try {
        return await invoke(name, args);
      } catch (err) {
        last = err;
      }
    }
    throw last || new Error("no command available: " + names.join(", "));
  }

  /** Decide where the row hints come from, then load them. */
  async function loadStatus() {
    statusMap = new Map();
    if (!invoke) {
      statusMap = fixtureStatus();
      statusMode = "sample";
      return;
    }

    // The backend is authoritative about its own mode: only a backend that
    // says it is in mock mode may have its rows labelled from the fixture.
    let meta = null;
    try {
      meta = await invoke("app_metadata");
    } catch (err) {
      console.warn("CodexBar: app_metadata unavailable", err);
    }
    if (meta && (meta.mockMode === true || meta.mode === "mock")) {
      statusMap = fixtureStatus();
      statusMode = "sample";
      return;
    }

    statusMode = "live";

    // The live per-provider state. `get_report` is the same payload the popover
    // renders, so the drawer and the menu can never disagree about a provider.
    let report = null;
    try {
      report = await invokeAny(["get_report", "usage_snapshot"]);
    } catch (err) {
      console.warn("CodexBar: get_report unavailable", err);
    }
    if (!report || !Array.isArray(report.providers) || !report.providers.length) {
      // This window can open before the first fetch lands; ask for one so a
      // row is never left without a state to show.
      try {
        report = await invokeAny(["refresh_now"]);
      } catch (err) {
        console.warn("CodexBar: refresh_now unavailable", err);
      }
    }
    if (report && Array.isArray(report.providers)) {
      report.providers.forEach((p) => statusMap.set(p.provider, snapshotEntry(p)));
    }

    // `session_states` carries the honest session vocabulary the frozen
    // FetchStatus cannot express (token expired vs fetch failed).
    try {
      const sessions = await invoke("session_states");
      const list = sessions && Array.isArray(sessions.sessions) ? sessions.sessions : [];
      list.forEach((s) => {
        const entry = statusMap.get(s.provider) || snapshotEntry({ provider: s.provider });
        entry.sessionState = s.sessionState || null;
        entry.tokenExpired = !!s.tokenExpired;
        entry.canReauth = !!s.canReauth;
        if (s.detail) entry.error = s.detail;
        statusMap.set(s.provider, entry);
      });
    } catch (err) {
      console.warn("CodexBar: session_states unavailable", err);
    }
  }

  /** One status hint for a provider, derived from `statusMap`. */
  function hintFor(id) {
    const entry = statusMap.get(id);
    if (!entry) return null;

    if (entry.tokenExpired || entry.sessionState === "expired") {
      return {
        tone: "status-warn",
        text: "● Token expired · sign in again",
        title: entry.error || "The stored credential is no longer usable.",
      };
    }

    switch (entry.code) {
      case "error":
        return {
          tone: "status-err",
          text: "● " + (entry.error || "Fetch failed"),
          title: entry.error || "The usage fetch failed.",
        };
      case "unconfigured":
        return {
          tone: "status-warn",
          text: "● " + (entry.error || "Not configured"),
          title: entry.error || "No credentials configured for this provider.",
        };
      case "stale":
        return {
          tone: "status-warn",
          text: "● Stale data",
          title: entry.error || "Showing last known values.",
        };
      case "empty":
        return { tone: "status-warn", text: "● No usage reported", title: "" };
      default: {
        const has = entry.used !== null && entry.used !== undefined;
        const pc = has ? (F ? F.pct(entry.used) : entry.used + "%") : "";
        return {
          tone: "status-ok",
          text: "● " + (has ? pc + " used" : "reporting"),
          title: has ? pc + " used (worst window)" : "",
        };
      }
    }
  }

  function titleFor(id) {
    const catalog = info && Array.isArray(info.catalog) ? info.catalog : null;
    const fromCatalog = catalog && catalog.find((c) => c.id === id);
    if (fromCatalog && fromCatalog.title) return fromCatalog.title;
    const known = PROVIDER_SET.find((p) => p.id === id);
    return known ? known.title : id;
  }

  /** Repaint one provider row from `entry` without touching the rest of the
   *  list — the click target stays in the DOM, so focus and pointer state are
   *  preserved. */
  function paintProviderRow(row, entry) {
    if (!row || !entry) return;
    row.classList.toggle("is-off", !entry.enabled);
    const toggle = row.querySelector(".switch");
    if (toggle) {
      toggle.setAttribute("aria-checked", String(entry.enabled));
      toggle.title = (entry.enabled ? "Disable " : "Enable ") + titleFor(entry.id);
    }
  }

  /** Re-sync every rendered row against the live `settings`. Rows are looked up
   *  by `data-provider` id, so a row can never keep pointing at a provider
   *  object from a previous `settings` (the bug: a click after a save mutated a
   *  detached entry and the change was never sent to the backend). */
  function syncProviderRows() {
    if (!settings) return;
    els.providerList.querySelectorAll(".provider-row[data-provider]").forEach((row) => {
      const entry = settings.providers.find((p) => p.id === row.dataset.provider);
      if (entry) paintProviderRow(row, entry);
    });
    paintCounts();
  }

  function paintCounts() {
    const on = settings.providers.filter((p) => p.enabled).length;
    els.navCount.textContent = on + " on";
    els.navCount.title = on + " of " + settings.providers.length + " providers enabled";
    els.providerSummary.textContent = on + " of " + settings.providers.length + " enabled";
  }

  function renderProviders() {
    const filter = (els.providerFilter.value || "").trim().toLowerCase();
    els.providerList.textContent = "";

    let visible = 0;
    settings.providers.forEach((entry, index) => {
      const meta = PROVIDER_SET.find((p) => p.id === entry.id) || {
        id: entry.id,
        title: titleFor(entry.id),
        sub: "",
      };
      const haystack = (meta.title + " " + meta.sub + " " + entry.id).toLowerCase();
      if (filter && !haystack.includes(filter)) return;
      visible += 1;

      const row = document.createElement("div");
      row.className = "provider-row" + (entry.enabled ? "" : " is-off");
      row.dataset.provider = entry.id;

      const logo = document.createElement("span");
      logo.className = "provider-logo";
      const img = document.createElement("img");
      img.src = "assets/" + entry.id + ".svg";
      img.alt = "";
      img.onerror = () => {
        img.remove();
        logo.textContent = meta.title.slice(0, 1);
      };
      logo.appendChild(img);
      row.appendChild(logo);

      const text = document.createElement("div");
      text.className = "provider-text";
      const name = document.createElement("div");
      name.className = "provider-name";
      name.textContent = meta.title;
      const sub = document.createElement("div");
      sub.className = "provider-sub";
      const hint = hintFor(entry.id);
      if (hint) {
        const bit = document.createElement("span");
        bit.className = "provider-hint " + hint.tone;
        bit.textContent = hint.text;
        if (hint.title) bit.title = hint.title;
        sub.appendChild(bit);
        const tag = document.createElement("span");
        tag.className = "provider-mode " + statusMode;
        tag.textContent = statusMode;
        tag.title = statusMode === "live"
          ? "State read from the running app (get_report / session_states)."
          : "State from the bundled sample fixture (ui/mock.js).";
        sub.appendChild(tag);
      } else {
        sub.textContent = meta.sub;
      }
      text.appendChild(name);
      text.appendChild(sub);
      row.appendChild(text);

      const order = document.createElement("div");
      order.className = "provider-order";
      const up = document.createElement("button");
      up.type = "button";
      up.textContent = "↑";
      up.title = "Move " + meta.title + " up";
      up.setAttribute("aria-label", "Move " + meta.title + " up");
      up.disabled = index === 0;
      up.addEventListener("click", () => move(entry.id, -1));
      const down = document.createElement("button");
      down.type = "button";
      down.textContent = "↓";
      down.title = "Move " + meta.title + " down";
      down.setAttribute("aria-label", "Move " + meta.title + " down");
      down.disabled = index === settings.providers.length - 1;
      down.addEventListener("click", () => move(entry.id, 1));
      order.appendChild(up);
      order.appendChild(down);
      row.appendChild(order);

      const toggle = document.createElement("button");
      toggle.className = "switch";
      toggle.setAttribute("role", "switch");
      toggle.setAttribute("aria-checked", String(entry.enabled));
      toggle.setAttribute("aria-label", "Enable " + meta.title);
      toggle.title = (entry.enabled ? "Disable " : "Enable ") + meta.title;
      toggle.addEventListener("click", () => {
        // Resolve against the *live* settings by id — never through the `entry`
        // captured at render time, which a `save()` echo may have detached.
        const current = settings.providers.find((p) => p.id === entry.id);
        if (!current) return;
        current.enabled = toggle.getAttribute("aria-checked") !== "true";
        // Paint the one row in place (no DOM rebuild, no focus loss) and let
        // `patch` re-sync against the authoritative echo when it lands.
        paintProviderRow(row, current);
        paintCounts();
        patch({ providers: settings.providers });
      });
      row.appendChild(toggle);

      els.providerList.appendChild(row);
    });

    if (!visible) {
      const empty = document.createElement("div");
      empty.className = "provider-row";
      empty.textContent = "No provider matches “" + els.providerFilter.value + "”.";
      els.providerList.appendChild(empty);
    }

    const on = settings.providers.filter((p) => p.enabled).length;
    els.navCount.textContent = on + " on";
    els.navCount.title = on + " of " + settings.providers.length + " providers enabled";
    els.providerSummary.textContent = on + " of " + settings.providers.length + " enabled";
  }

  function move(id, delta) {
    const list = settings.providers;
    const index = list.findIndex((p) => p.id === id);
    if (index < 0) return;
    const target = index + delta;
    if (target < 0 || target >= list.length) return;
    const [item] = list.splice(index, 1);
    list.splice(target, 0, item);
    patch({ providers: list });
    renderProviders();
  }

  /* ------------------------------------------------------------------ */
  /* misc                                                                 */
  /* ------------------------------------------------------------------ */

  let toastTimer = null;
  function toast(message) {
    if (!message) return;
    els.toast.textContent = message;
    els.toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      els.toast.hidden = true;
    }, 2800);
  }

  function renderBackendLine() {
    if (!els.backend) return;
    const on = settings.providers.filter((p) => p.enabled).length;
    if (backend === "tauri") {
      els.backend.textContent =
        "Live: get_settings / set_settings → " + on + " of " + settings.providers.length +
        " providers enabled, " + settings.refreshIntervalSecs + "s cadence.";
      return;
    }
    els.backend.textContent =
      "No Tauri bridge detected — the same payload is kept in localStorage so this " +
      "window works in a plain browser. The tray app is unaffected.";
  }

  function renderInfo() {
    if (els.autostartInfo) {
      if (info) {
        els.autostartInfo.textContent = info.autostartEnabled
          ? "Registered at sign-in: " + (info.autostartCommand || "(value present)")
          : "Not registered — no CodexBar entry under HKCU\\…\\CurrentVersion\\Run.";
      } else {
        els.autostartInfo.textContent =
          "Startup registration is applied by the tray app; this window only reads it back.";
      }
    }
    if (els.pathsInfo) {
      els.pathsInfo.textContent = info
        ? "config " + (info.configPath || "—") + "\nexe " + (info.exePath || "—")
        : "Paths are reported by the backend (settings_info).";
    }
  }

  function selectPane(name) {
    els.paneButtons.forEach((btn) =>
      btn.setAttribute("aria-current", String(btn.dataset.pane === name))
    );
    els.panes.forEach((pane) => pane.classList.toggle("active", pane.id === "pane-" + name));
    if (els.sub) {
      els.sub.textContent = "Settings · " + name.charAt(0).toUpperCase() + name.slice(1);
    }
  }

  /* ------------------------------------------------------------------ */
  /* boot                                                                 */
  /* ------------------------------------------------------------------ */

  /** The non-provider controls (switches + interval). Safe to call on every
   *  authoritative update: each one paints in place. */
  function paintControls() {
    paintSwitch("set-startAtLogin", settings.startAtLogin);
    paintSwitch("set-mergeIcons", settings.mergeIcons);
    paintSwitch("set-refreshCredentials", settings.refreshCredentials);
    paintSwitch("set-showPercentInIcon", settings.showPercentInIcon);

    const interval = document.getElementById("set-refreshIntervalSecs");
    const wanted = String(settings.refreshIntervalSecs);
    if (![...interval.options].some((o) => o.value === wanted)) {
      const custom = document.createElement("option");
      custom.value = wanted;
      custom.textContent = wanted + " s";
      interval.appendChild(custom);
    }
    interval.value = wanted;
  }

  async function paintAll() {
    paintControls();
    renderProviders();
    renderInfo();
    renderBackendLine();
  }

  async function boot() {
    settings = await load();
    if (invoke) {
      try {
        info = await invoke("settings_info");
      } catch (err) {
        console.warn("CodexBar: settings_info unavailable", err);
      }
    }
    if (!info) info = null;

    await loadStatus();
    await paintAll();

    bindSwitch("set-startAtLogin", (on) => {
      patch({ startAtLogin: on });
      renderInfo();
    }, (on) => (on ? "CodexBar will start with Windows." : "CodexBar will no longer start with Windows."));

    bindSwitch("set-mergeIcons", (on) => patch({ mergeIcons: on }));
    bindSwitch("set-refreshCredentials", (on) => patch({ refreshCredentials: on }));
    bindSwitch("set-showPercentInIcon", (on) => patch({ showPercentInIcon: on }));

    const interval = document.getElementById("set-refreshIntervalSecs");
    interval.addEventListener("change", () => {
      patch({ refreshIntervalSecs: Number(interval.value) });
    });

    els.providerFilter.addEventListener("input", renderProviders);

    els.paneButtons.forEach((btn) =>
      btn.addEventListener("click", () => selectPane(btn.dataset.pane))
    );

    // Deep link: settings.html#providers opens that pane (also how the evidence
    // renders are produced without clicking).
    const wantedPane = (window.location.hash || "").replace("#", "");
    if (wantedPane && document.getElementById("pane-" + wantedPane)) selectPane(wantedPane);

    document.getElementById("btnRefreshNow").addEventListener("click", async () => {
      if (!invoke) {
        toast("Refresh is only available in the desktop app.");
        return;
      }
      try {
        const report = await invoke("refresh_now");
        const n = report && report.providers ? report.providers.length : 0;
        toast("Refreshed " + n + " providers.");
      } catch (err) {
        console.warn("CodexBar: refresh_now failed", err);
        toast("Refresh failed.");
      }
    });

    document.getElementById("btnQuit").addEventListener("click", () => {
      if (invoke) invoke("quit").catch(() => invoke("quit_app").catch(() => {}));
      else toast("Quit is only available in the desktop app.");
    });

    document.getElementById("btnCopySettings").addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(JSON.stringify(settings, null, 2));
        toast("Settings JSON copied.");
      } catch (err) {
        toast("Clipboard unavailable — settings kept internally.");
      }
    });

    document.getElementById("btnResetSettings").addEventListener("click", async () => {
      let defaults = normalize(null);
      if (invoke) {
        try {
          defaults = normalize(await invoke("default_settings"));
        } catch (err) {
          console.warn("CodexBar: default_settings unavailable", err);
        }
      }
      await save(defaults);
      await paintAll();
      toast("Settings reset to defaults.");
    });

    // The backend pushes the settings it just put in force (its own
    // `set_settings` echo, and every change made from the tray menu) — adopt it
    // so the window can never keep painting a stale list, whatever triggered the
    // write. `adoptRemote` re-syncs in place, so this does not clobber a click.
    const listen = TAURI && TAURI.event ? TAURI.event.listen : null;
    if (listen) {
      try {
        await listen("settings-updated", (event) => adoptRemote(event.payload));
      } catch (err) {
        console.warn("CodexBar: settings-updated listener unavailable", err);
      }
    }

    if (invoke) {
      invoke("app_metadata")
        .then((m) => {
          if (!m) return;
          if (m.name) els.aboutName.textContent = m.name;
          els.aboutVersion.textContent = "v" + m.version + " · schema v" + m.schemaVersion;
        })
        .catch(() => {});
    }
    els.aboutSource.textContent = backend === "tauri" ? "backend connected" : "local storage";
  }

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && invoke) {
      invoke("hide_settings").catch(() => {});
    }
  });

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", boot);
  } else {
    boot();
  }
})();
