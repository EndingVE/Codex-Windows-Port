/* CodexBar for Windows — popover controller.
 *
 * Data sources, in order of preference:
 *   1. the Tauri backend — `invoke("get_report")` (legacy alias
 *      `usage_snapshot`) plus the `usage-updated` event pushed by the
 *      background refresh tick;
 *   2. `window.__CODEXBAR_MOCK__` — the static fixture in `mock.js`, which is a
 *      byte-for-byte dump of `codexbar usage --format json`.
 *
 * Both paths hand this file the same `UsageReport` shape (see
 * `crates/codexbar-core/src/types.rs`), so this file never knows which provider
 * a value came from. Command names are probed, not assumed: the backend may
 * expose `get_report`/`get_settings` or the older `usage_snapshot` naming, and
 * in a plain browser none of them exist — the fixture still renders.
 */
(function () {
  "use strict";

  const TAURI = window.__TAURI__ || null;
  const invoke = TAURI && TAURI.core ? TAURI.core.invoke : null;
  const listen = TAURI && TAURI.event ? TAURI.event.listen : null;

  const F = window.CodexBarFormat;
  const R = window.CodexBarRender;

  const els = {
    cards: document.getElementById("cards"),
    brandSub: document.getElementById("brandSub"),
    brandMark: document.getElementById("brandMark"),
    modeBanner: document.getElementById("modeBanner"),
    modeText: document.getElementById("modeText"),
    legend: document.getElementById("legend"),
    footLeft: document.getElementById("footLeft"),
    footRight: document.getElementById("footRight"),
    btnRefresh: document.getElementById("btnRefresh"),
    btnSettings: document.getElementById("btnSettings"),
    btnClose: document.getElementById("btnClose"),
    cmdRefresh: document.getElementById("cmdRefresh"),
    cmdSettings: document.getElementById("cmdSettings"),
    cmdAbout: document.getElementById("cmdAbout"),
    cmdQuit: document.getElementById("cmdQuit"),
    aboutVersion: document.getElementById("aboutVersion"),
    toast: document.getElementById("toast"),
  };

  /** Current report being rendered. */
  let report = null;
  /** True when we are showing the embedded fixture rather than backend data. */
  let usingFixture = false;
  /** Backend metadata, when the commands exist. */
  let meta = null;

  /* ------------------------------------------------------------------ */
  /* evidence hooks — query-string only                                   */
  /* ------------------------------------------------------------------ */

  /* Used by the static evidence renders (see evidence/v3-notes.md); the
   * shipping Tauri window sets none of these, so the app always infers the
   * mode from the data it was actually handed.
   *
   *   ?mode=sample        pin the sample-data labelling (headers/footer/banner)
   *   ?mode=live          pin the live labelling
   *   ?backend=mock        serve window.__CODEXBAR_MOCK__ as *backend* data
   *                       (usingFixture=false) — the exact shape the reviewer
   *                       reported: a backend answering with mock sources.
   */
  const PARAMS = new URLSearchParams(window.location.search);

  /** Which of the two labels a report deserves.
   *
   *  Sample wins whenever any of these is true: the page is the embedded
   *  fixture, `?mode=sample` was pinned, or — the defect this fixes — every
   *  card the backend returned carries `source: "mock"`. A report is only
   *  "live" when at least one card really came from a live fetch. */
  function dataMode(providers) {
    const pinned = PARAMS.get("mode");
    if (pinned === "sample" || pinned === "mock") return "sample";
    if (pinned === "live") return "live";
    if (usingFixture) return "sample";
    const list = providers || [];
    if (!list.length) return "live";
    return list.some((p) => p.source && p.source !== "mock") ? "live" : "sample";
  }

  /* ------------------------------------------------------------------ */
  /* command plumbing                                                     */
  /* ------------------------------------------------------------------ */

  /** Try each command name in order; the first that resolves wins. */
  async function invokeAny(names, args) {
    if (!invoke) throw new Error("no tauri bridge");
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

  function openSettings() {
    if (!invoke) {
      toast("Settings window is only available in the desktop app.");
      return;
    }
    invokeAny(["open_settings"]).catch((err) => {
      console.warn("CodexBar: open_settings failed", err);
      toast("Settings unavailable — backend command missing.");
    });
  }

  /** Exposed so the "Open Settings" action inside a card note can reach it. */
  window.__CODEXBAR_OPEN_SETTINGS__ = openSettings;

  function quitApp() {
    if (!invoke) {
      toast("Quit is only available in the desktop app.");
      return;
    }
    invokeAny(["quit"]).catch((err) => console.warn("CodexBar: quit failed", err));
  }

  function hidePopover() {
    if (!invoke) return;
    invokeAny(["hide_popover", "close_popover"]).catch(() => {});
  }

  let toastTimer = null;
  function toast(message) {
    els.toast.textContent = message;
    els.toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      els.toast.hidden = true;
    }, 2600);
  }

  /* ------------------------------------------------------------------ */
  /* fixture handling                                                     */
  /* ------------------------------------------------------------------ */

  /** Shift the static fixture's timestamps onto "now" so countdowns read live.
   *  Only ever applied to the embedded mock; backend data is used as-is. */
  function rebaseFixture(payload) {
    const anchor = Date.parse(payload.generatedAt);
    if (!Number.isFinite(anchor)) return payload;
    const delta = Date.now() - anchor;
    if (Math.abs(delta) < 1500) return payload;

    const shift = (iso) => (iso ? new Date(Date.parse(iso) + delta).toISOString() : iso);
    return {
      ...payload,
      generatedAt: shift(payload.generatedAt),
      providers: payload.providers.map((p) => ({
        ...p,
        fetchedAt: shift(p.fetchedAt),
        windows: (p.windows || []).map((w) => ({
          ...w,
          window: { ...w.window, resetsAt: shift(w.window.resetsAt) },
        })),
      })),
    };
  }

  /* ------------------------------------------------------------------ */
  /* rendering                                                            */
  /* ------------------------------------------------------------------ */

  function plural(count, singular, pluralWord) {
    return count + " " + (count === 1 ? singular : pluralWord);
  }

  function renderHeader(providers) {
    const sample = dataMode(providers) === "sample";
    const reporting = providers.filter(
      (p) => p.status === "ok" && F.headline(p) !== null
    );

    if (!reporting.length) {
      els.brandSub.textContent = plural(providers.length, "provider", "providers") +
        (sample ? " · no sample data" : " · no live data");
      els.brandMark.style.background =
        "conic-gradient(var(--surface-4) 0turn 1turn)";
      els.brandMark.title = sample
        ? "No sample data to summarise"
        : "No live data to summarise";
      return;
    }

    const worst = reporting.reduce((a, b) => (F.headline(b) > F.headline(a) ? b : a));
    const value = F.headline(worst);
    const severity = F.severityFor(value);

    // The count is label-agnostic; the qualifier is what must never lie. In
    // sample mode it reads "(sample data)" — the word "live" is reserved for a
    // report that really contains a live fetch.
    els.brandSub.textContent = "";
    els.brandSub.append(
      document.createTextNode(worst.title + " "),
      Object.assign(document.createElement("b"), { textContent: F.pct(value) + " used" }),
      document.createTextNode(
        " · " + plural(reporting.length, "provider", "providers") +
        (sample ? " (sample data)" : " live")
      )
    );
    els.brandSub.title =
      worst.title + " is the most constrained provider at " + F.pct(value) + " used (" +
      F.SEVERITY_COPY[severity] + "). " + plural(reporting.length, "provider", "providers") +
      (sample ? " reporting sample data." : " reporting live data.");

    els.brandMark.style.background =
      "conic-gradient(var(--" + severity + ") 0turn " + value / 100 + "turn, " +
      "var(--surface-4) " + value / 100 + "turn 1turn)";
    els.brandMark.title = F.pct(value) + " used — " + F.SEVERITY_COPY[severity];
  }

  function renderFooter(providers) {
    const errored = providers.filter((p) => p.status === "error").length;
    const unconfigured = providers.filter((p) => p.status === "notConfigured").length;

    els.footLeft.textContent = "";
    els.footLeft.append(
      document.createTextNode("v" + report.schemaVersion + " · " +
        plural(providers.length, "provider", "providers"))
    );
    if (errored) {
      els.footLeft.append(
        document.createTextNode(" · "),
        Object.assign(document.createElement("span"), {
          className: "flag-err",
          textContent: plural(errored, "error", "errors"),
        })
      );
    }
    if (unconfigured) {
      els.footLeft.append(
        document.createTextNode(" · "),
        Object.assign(document.createElement("span"), {
          className: "flag-unset",
          textContent: plural(unconfigured, "unconfigured", "unconfigured"),
        })
      );
    }

    const stamp = F.clockLabel(report.generatedAt);
    const sample = dataMode(providers) === "sample";
    els.footRight.textContent = "";
    els.footRight.append(
      Object.assign(document.createElement("span"), {
        className: sample ? "mock" : "live",
        textContent: sample ? "sample" : "live",
      }),
      document.createTextNode(" " + stamp)
    );
    els.footRight.title = (sample
      ? "Sample data (no live fetch) — "
      : "Fetched from the local backend — ") + stamp;
  }

  function renderBanner(providers) {
    const allMockSources = providers.length > 0 && providers.every((p) => p.source === "mock");
    const sample = dataMode(providers) === "sample";
    const showBanner = usingFixture || allMockSources || sample;

    els.modeBanner.hidden = !showBanner;
    if (!showBanner) return;

    els.modeBanner.classList.remove("is-live");
    if (usingFixture) {
      els.modeText.textContent =
        "Sample data — no credentials read, no network calls.";
      els.modeText.title = meta
        ? meta.name + " v" + meta.version + ", schema v" + meta.schemaVersion
        : "Rendered from ui/mock.js";
    } else {
      els.modeText.textContent =
        "Backend serving sample data — no credentials read, no network calls.";
      els.modeText.title = "Provider sources in this report: " +
        [...new Set(providers.map((p) => p.source))].join(", ");
    }
  }

  function render() {
    if (!report) return;
    els.cards.textContent = "";

    const ordered = (report.providers || []).slice();
    if (!ordered.length) {
      els.cards.appendChild(R.el("p", "placeholder", "No providers registered."));
      els.footLeft.textContent = "v" + report.schemaVersion + " · no providers";
      els.footRight.textContent = "—";
      els.brandSub.textContent = "no providers";
      return;
    }
    ordered.forEach((p) => els.cards.appendChild(R.renderCard(p)));

    renderHeader(ordered);
    renderFooter(ordered);
    renderBanner(ordered);
  }

  /** Legend is static — build it once. */
  els.legend.replaceWith(Object.assign(R.renderLegend(), { id: "legend" }));

  /** Display → "Show the colour legend" lives in uiPrefs, so the popover and
   *  the settings window agree without a round-trip through Rust. */
  function applyLegendPref() {
    const foot = document.getElementById("cardsFoot");
    if (foot) foot.hidden = !F.uiPrefs.get("showLegend", true);
  }
  applyLegendPref();
  window.addEventListener("storage", applyLegendPref);

  /* ------------------------------------------------------------------ */
  /* data flow                                                            */
  /* ------------------------------------------------------------------ */

  function adopt(payload, fromFixture) {
    usingFixture = !!fromFixture;
    report = fromFixture ? rebaseFixture(payload) : payload;
    render();
  }

  async function boot() {
    if (window.__CODEXBAR_HARNESS_REPORT__) {
      // Static evidence harness (evidence/v3-harness-live.html): a real
      // captured payload rendered exactly like backend data, so the per-card
      // `source` values — not a flag — decide the live/sample labelling.
      adopt(window.__CODEXBAR_HARNESS_REPORT__, false);
      return;
    }
    if (invoke) {
      try {
        const payload = await invokeAny(["get_report", "usage_snapshot", "get_usage_report"]);
        adopt(payload, false);
        invokeAny(["app_metadata"])
          .then((m) => {
            meta = m;
            if (m && m.version) els.aboutVersion.textContent = "v" + m.version;
            render();
          })
          .catch(() => {});
        if (listen) {
          try {
            await listen("usage-updated", (event) => adopt(event.payload, false));
          } catch (err) {
            console.warn("CodexBar: usage-updated listener unavailable", err);
          }
        }
        return;
      } catch (err) {
        console.warn("CodexBar: backend unavailable, falling back to fixture", err);
      }
    }
    if (window.__CODEXBAR_MOCK__) {
      // ?backend=mock serves the fixture through the *backend* code path so the
      // static evidence render can exercise the "backend answering with sample
      // data" branch (see the evidence hooks above).
      const asBackend = PARAMS.get("backend") === "mock";
      adopt(window.__CODEXBAR_MOCK__, !asBackend);
    } else {
      els.cards.textContent = "";
      els.cards.appendChild(R.el("p", "placeholder", "No data source available."));
    }
  }

  async function refresh() {
    els.btnRefresh.classList.add("spinning");
    try {
      if (invoke) {
        adopt(await invokeAny(["refresh_now"]), false);
      } else if (window.__CODEXBAR_MOCK__) {
        adopt(window.__CODEXBAR_MOCK__, true);
        toast("Refreshed sample data.");
      }
    } catch (err) {
      console.warn("CodexBar: refresh failed", err);
      toast("Refresh failed.");
    } finally {
      setTimeout(() => els.btnRefresh.classList.remove("spinning"), 400);
    }
  }

  /* ------------------------------------------------------------------ */
  /* interaction                                                          */
  /* ------------------------------------------------------------------ */

  els.btnRefresh.addEventListener("click", refresh);
  els.cmdRefresh.addEventListener("click", refresh);
  els.btnSettings.addEventListener("click", openSettings);
  els.cmdSettings.addEventListener("click", openSettings);
  els.btnClose.addEventListener("click", hidePopover);
  els.cmdQuit.classList.add("danger");
  els.cmdQuit.addEventListener("click", quitApp);
  els.cmdAbout.addEventListener("click", () => {
    toast(
      meta
        ? meta.name + " v" + meta.version + " · schema v" + meta.schemaVersion
        : "CodexBar for Windows · schema v" + (report ? report.schemaVersion : 1)
    );
  });

  document.addEventListener("keydown", (event) => {
    const ctrl = event.ctrlKey || event.metaKey;
    if (event.key === "Escape") {
      hidePopover();
      return;
    }
    if (ctrl && (event.key === "q" || event.key === "Q")) {
      event.preventDefault();
      quitApp();
      return;
    }
    if (ctrl && event.key === ",") {
      event.preventDefault();
      openSettings();
      return;
    }
    if (ctrl && (event.key === "r" || event.key === "R")) {
      event.preventDefault();
      refresh();
      return;
    }
    if (!ctrl && (event.key === "s" || event.key === "S")) openSettings();
    if (!ctrl && (event.key === "r" || event.key === "R")) refresh();
  });

  // Countdowns drift, so re-render periodically while the popover is open.
  setInterval(render, 30000);

  document.addEventListener("DOMContentLoaded", boot);
  if (document.readyState !== "loading") boot();
})();
