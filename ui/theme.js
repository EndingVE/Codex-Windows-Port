/* CodexBar for Windows — appearance runtime (theme, accent, scale, density,
 * opacity). Shared by every surface: the popover (index.html), the settings
 * window (settings.html), the states gallery and the floating widget.
 *
 * Load it as a *blocking* <script> in <head>, right after styles.css and
 * before <body>: it paints the cached appearance synchronously, so the first
 * frame already has the right theme (no dark→light flash), then asks the
 * backend for the authoritative copy and listens for `appearance-changed`.
 *
 * Stable public API (the widget consumes it read-only):
 *
 *   window.CodexBarTheme.apply(appearance)   → normalised appearance
 *       Write the CSS custom properties / data attributes on <html>. Accepts a
 *       full or partial object; missing / invalid fields fall back to DEFAULTS.
 *   window.CodexBarTheme.current()           → last applied appearance
 *   window.CodexBarTheme.normalize(obj)      → clamped copy, no side effects
 *   window.CodexBarTheme.onChange(fn)        → unsubscribe()
 *       Called after every apply (local or from another window).
 *   window.CodexBarTheme.save(patch)         → Promise<appearance>
 *       Persist through `set_appearance` (falls back to localStorage in a
 *       plain browser). The panel uses it; the widget must not.
 *   window.CodexBarTheme.DEFAULTS / PRESETS / contrast() / textOn()
 *
 * What apply() sets on <html> (documented contract for widget.css):
 *   data-theme="light|dark"            resolved theme (system → media query)
 *   data-theme-pref="system|light|dark"
 *   data-density="compact|normal"
 *   --accent, --accent-hover, --accent-dim, --accent-soft, --on-accent,
 *   --accent-text (accent tuned to ≥ 4.5:1 on --surface, for links/text),
 *   --on-accent-dim (text on --accent-dim, ≥ 4.5:1),
 *   --ui-scale (e.g. 1.1), --font-scale-pct (e.g. 110),
 *   --popover-opacity, --widget-opacity (0.6 – 1),
 *   --density-gap (px multiplier: 0.75 compact / 1 normal).
 * Every surface colour (--bg, --surface*, --border*, --text, --muted, --dim,
 * severity colours) comes from styles.css, keyed on data-theme.
 */
(function () {
  "use strict";

  const DEFAULTS = Object.freeze({
    version: 1,
    theme: "system",
    accent: "#6ea8fe",
    fontScale: 100,
    density: "normal",
    popoverOpacity: 1,
    widgetOpacity: 1,
    showLegend: true,
  });

  /** Accent presets offered by the panel (the free picker allows any hex). */
  const PRESETS = Object.freeze([
    { id: "blue", label: "Blue", hex: "#6ea8fe" },
    { id: "violet", label: "Violet", hex: "#a78bfa" },
    { id: "pink", label: "Pink", hex: "#f472b6" },
    { id: "orange", label: "Orange", hex: "#fb923c" },
    { id: "green", label: "Green", hex: "#34d399" },
    { id: "teal", label: "Teal", hex: "#2dd4bf" },
    { id: "graphite", label: "Graphite", hex: "#94a3b8" },
  ]);

  const LIMITS = Object.freeze({
    fontScale: [90, 130],
    opacity: [0.6, 1],
  });

  /** Card surface per theme (must match --surface in styles.css). The accent
   *  text variant is tuned against these. */
  const SURFACE = { dark: "#16191f", light: "#ffffff" };
  /** Page background per theme (for the accent-dim pill + first paint). */
  const BG = { dark: "#0d0f13", light: "#f3f4f7" };

  const CACHE_KEY = "codexbar.appearance";
  const LEGACY_LEGEND_KEY = "codexbar.ui.showLegend";

  /* ---------- colour maths (WCAG 2.x) ------------------------------------ */

  function normalizeHex(raw) {
    if (typeof raw !== "string") return null;
    let hex = raw.trim().replace(/^#/, "");
    if (!/^[0-9a-fA-F]+$/.test(hex)) return null;
    if (hex.length === 3) hex = hex.split("").map((c) => c + c).join("");
    if (hex.length !== 6) return null;
    return "#" + hex.toLowerCase();
  }

  function toRgb(hex) {
    const n = normalizeHex(hex) || DEFAULTS.accent;
    return [1, 3, 5].map((i) => parseInt(n.slice(i, i + 2), 16));
  }

  function toHex(rgb) {
    return (
      "#" +
      rgb
        .map((v) => Math.max(0, Math.min(255, Math.round(v))).toString(16).padStart(2, "0"))
        .join("")
    );
  }

  function luminance(hex) {
    const [r, g, b] = toRgb(hex).map((v) => {
      const s = v / 255;
      return s <= 0.03928 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
    });
    return 0.2126 * r + 0.7152 * g + 0.0722 * b;
  }

  /** WCAG contrast ratio between two colours (1 – 21). */
  function contrast(a, b) {
    const la = luminance(a);
    const lb = luminance(b);
    return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
  }

  function mix(a, b, t) {
    const ca = toRgb(a);
    const cb = toRgb(b);
    return toHex(ca.map((v, i) => v + (cb[i] - v) * t));
  }

  const INK_DARK = "#000000";
  const INK_LIGHT = "#ffffff";

  /** Best text colour on `bg`: black or white, whichever contrasts more. For
   *  any sRGB colour one of the two always reaches ≥ 4.58:1 (the worst case is
   *  a mid grey at relative luminance ≈ 0.18); a tinted near-black would not. */
  function textOn(bg) {
    return contrast(INK_DARK, bg) >= contrast(INK_LIGHT, bg) ? INK_DARK : INK_LIGHT;
  }

  /** Move `color` towards black/white until it reaches `ratio` on `bg`. */
  function ensureContrast(color, bg, ratio) {
    if (contrast(color, bg) >= ratio) return normalizeHex(color);
    const target = luminance(bg) > 0.18 ? "#000000" : "#ffffff";
    for (let t = 0.05; t <= 1.0001; t += 0.05) {
      const candidate = mix(color, target, t);
      if (contrast(candidate, bg) >= ratio) return candidate;
    }
    return target;
  }

  /* ---------- normalisation (mirrors appearance.rs) --------------------- */

  function clampNum(value, [lo, hi], fallback) {
    const n = Number(value);
    if (!Number.isFinite(n)) return fallback;
    return Math.min(hi, Math.max(lo, n));
  }

  function normOpacity(value, fallback) {
    let n = Number(value);
    if (!Number.isFinite(n)) return fallback;
    if (n > 1) n = n / 100;
    return Math.round(clampNum(n, LIMITS.opacity, fallback) * 100) / 100;
  }

  function normalize(input) {
    const src = input && typeof input === "object" ? input : {};
    const out = Object.assign({}, DEFAULTS);
    const theme = String(src.theme || "").toLowerCase();
    if (theme === "system" || theme === "light" || theme === "dark") out.theme = theme;
    out.accent = normalizeHex(src.accent) || DEFAULTS.accent;
    if (src.fontScale !== undefined) {
      let pct = Number(src.fontScale);
      if (Number.isFinite(pct)) {
        if (pct <= 3) pct *= 100;
        out.fontScale = Math.round(clampNum(pct, LIMITS.fontScale, DEFAULTS.fontScale));
      }
    }
    const density = String(src.density || "").toLowerCase();
    if (density === "compact" || density === "normal") out.density = density;
    if (src.popoverOpacity !== undefined) {
      out.popoverOpacity = normOpacity(src.popoverOpacity, DEFAULTS.popoverOpacity);
    }
    if (src.widgetOpacity !== undefined) {
      out.widgetOpacity = normOpacity(src.widgetOpacity, DEFAULTS.widgetOpacity);
    }
    if (typeof src.showLegend === "boolean") out.showLegend = src.showLegend;
    return out;
  }

  /* ---------- theme resolution ------------------------------------------ */

  const media =
    typeof window !== "undefined" && window.matchMedia
      ? window.matchMedia("(prefers-color-scheme: light)")
      : null;

  function resolveTheme(pref) {
    if (pref === "light" || pref === "dark") return pref;
    return media && media.matches ? "light" : "dark";
  }

  /** Every derived token for an appearance + resolved theme (pure). */
  function tokens(appearance, resolved) {
    const a = normalize(appearance);
    const theme = resolved || resolveTheme(a.theme);
    const surface = SURFACE[theme];
    const accent = a.accent;
    const hover = mix(accent, theme === "dark" ? "#ffffff" : "#000000", 0.14);
    const dim = mix(accent, BG[theme], theme === "dark" ? 0.55 : 0.78);
    return {
      theme,
      "--accent": accent,
      "--accent-hover": hover,
      "--accent-dim": dim,
      "--accent-soft": toHex(toRgb(accent)).replace(/^#/, "#") + "1f",
      "--on-accent": textOn(accent),
      "--on-accent-hover": textOn(hover),
      "--on-accent-dim": textOn(dim),
      "--accent-text": ensureContrast(accent, surface, 4.5),
      "--ui-scale": String(a.fontScale / 100),
      "--font-scale-pct": String(a.fontScale),
      "--popover-opacity": String(a.popoverOpacity),
      "--widget-opacity": String(a.widgetOpacity),
      "--density-gap": a.density === "compact" ? "0.75" : "1",
    };
  }

  /* ---------- apply ------------------------------------------------------ */

  let current = normalize(null);
  const listeners = new Set();

  function apply(appearance) {
    current = normalize(appearance);
    if (typeof document === "undefined" || !document.documentElement) return current;
    const root = document.documentElement;
    const t = tokens(current);
    root.setAttribute("data-theme", t.theme);
    root.setAttribute("data-theme-pref", current.theme);
    root.setAttribute("data-density", current.density);
    root.style.colorScheme = t.theme;
    Object.keys(t).forEach((key) => {
      if (key.startsWith("--")) root.style.setProperty(key, t[key]);
    });
    try {
      window.localStorage.setItem(CACHE_KEY, JSON.stringify(current));
    } catch (err) {
      /* no storage: the backend copy still arrives a few ms later */
    }
    listeners.forEach((fn) => {
      try {
        fn(current);
      } catch (err) {
        console.warn("CodexBar theme listener failed", err);
      }
    });
    return current;
  }

  function onChange(fn) {
    listeners.add(fn);
    return () => listeners.delete(fn);
  }

  /* ---------- backend bridge -------------------------------------------- */

  const TAURI = typeof window !== "undefined" ? window.__TAURI__ : null;
  const invoke = TAURI && TAURI.core ? TAURI.core.invoke : null;
  const listen = TAURI && TAURI.event ? TAURI.event.listen : null;

  function readCache() {
    try {
      const raw = window.localStorage.getItem(CACHE_KEY);
      return raw ? JSON.parse(raw) : null;
    } catch (err) {
      return null;
    }
  }

  /** The legacy localStorage `showLegend` (pre-appearance.json), if any. */
  function legacyLegend() {
    try {
      const raw = window.localStorage.getItem(LEGACY_LEGEND_KEY);
      return raw === null ? null : JSON.parse(raw) === true;
    } catch (err) {
      return null;
    }
  }

  async function save(patch) {
    const next = normalize(Object.assign({}, current, patch || {}));
    if (invoke) {
      const stored = await invoke("set_appearance", { appearance: next });
      return apply(stored);
    }
    // Plain browser (evidence renders): no backend, broadcast via storage.
    return apply(next);
  }

  async function sync() {
    if (!invoke) return current;
    try {
      let stored = await invoke("get_appearance", {
        resolvedTheme: resolveTheme("system"),
      });
      // One-time migration of the legacy localStorage legend preference.
      const legacy = legacyLegend();
      if (legacy === false && stored && stored.showLegend !== false) {
        stored = await invoke("set_appearance", {
          appearance: Object.assign({}, stored, { showLegend: false }),
        });
      }
      if (legacy !== null) {
        try {
          window.localStorage.removeItem(LEGACY_LEGEND_KEY);
        } catch (err) {
          /* ignore */
        }
      }
      return apply(stored);
    } catch (err) {
      console.warn("CodexBar: get_appearance unavailable", err);
      return current;
    }
  }

  /** `?theme=light|dark&accent=%23ff8800&scale=110&density=compact`
   *  overrides for headless evidence renders only. */
  function queryOverrides() {
    try {
      const q = new URLSearchParams(window.location.search || "");
      const out = {};
      if (q.get("theme")) out.theme = q.get("theme");
      if (q.get("accent")) out.accent = q.get("accent");
      if (q.get("scale")) out.fontScale = Number(q.get("scale"));
      if (q.get("density")) out.density = q.get("density");
      if (q.get("opacity")) out.popoverOpacity = Number(q.get("opacity"));
      return out;
    } catch (err) {
      return {};
    }
  }

  const api = {
    DEFAULTS,
    PRESETS,
    LIMITS,
    apply,
    contrast,
    current: () => current,
    ensureContrast,
    normalize,
    normalizeHex,
    onChange,
    resolveTheme,
    save,
    sync,
    textOn,
    tokens,
  };

  if (typeof window !== "undefined") {
    window.CodexBarTheme = api;

    // 1. Synchronous first paint from the cache (+ evidence overrides).
    const cached = readCache() || {};
    const legacy = legacyLegend();
    if (legacy !== null && cached.showLegend === undefined) cached.showLegend = legacy;
    apply(Object.assign({}, cached, queryOverrides()));

    // 2. `system` follows the OS live.
    if (media) {
      const onScheme = () => {
        if (current.theme === "system") apply(current);
      };
      if (media.addEventListener) media.addEventListener("change", onScheme);
      else if (media.addListener) media.addListener(onScheme);
    }

    // 3. Authoritative copy + live updates from any window.
    if (!Object.keys(queryOverrides()).length) sync();
    if (listen) {
      listen("appearance-changed", (event) => apply(event.payload)).catch((err) =>
        console.warn("CodexBar: appearance-changed listener unavailable", err)
      );
    }
    window.addEventListener("storage", (event) => {
      if (event.key === CACHE_KEY && event.newValue && !invoke) {
        try {
          apply(JSON.parse(event.newValue));
        } catch (err) {
          /* ignore */
        }
      }
    });
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
})();
