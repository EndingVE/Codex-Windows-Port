/* CodexBar for Windows — shared formatting + state classification.
 *
 * Loaded by the popover (index.html), the states gallery (states.html) and the
 * settings window (settings.html), so every surface agrees about severity
 * thresholds, duration copy and what a "token expired" card looks like.
 *
 * Mirrors the Rust projections in `crates/codexbar-core/src/types.rs`:
 *   severity_for_used_percent  → severityFor()
 *   humanize()                 → humanize()
 *   ProviderSnapshot::headline_window() → headlineWindow()
 *   RateWindow::display_clamped()       → clampPct()
 */
(function () {
  "use strict";

  /* ---------- severity ------------------------------------------------- */

  /** Same buckets as `severity_for_used_percent` in codexbar-core. */
  function severityFor(usedPercent) {
    if (usedPercent >= 90) return "critical";
    if (usedPercent >= 70) return "warning";
    return "healthy";
  }

  /** Human sentence for the legend / tooltips. Thresholds are used-percent. */
  const SEVERITY_COPY = {
    healthy: "under 70% used",
    warning: "70–89% used",
    critical: "90% used or more",
  };

  /* ---------- numbers + time ------------------------------------------- */

  function clampPct(value) {
    return Math.min(100, Math.max(0, Number(value) || 0));
  }

  function pct(value) {
    const rounded = Math.round((Number(value) || 0) * 10) / 10;
    return (Number.isInteger(rounded) ? rounded.toFixed(0) : rounded.toFixed(1)) + "%";
  }

  /** Compact duration, mirroring `humanize()` in codexbar-core. */
  function humanize(ms) {
    const secs = Math.max(0, Math.round(ms / 1000));
    const days = Math.floor(secs / 86400);
    const hours = Math.floor((secs % 86400) / 3600);
    const minutes = Math.floor((secs % 3600) / 60);
    if (days > 0) return days + "d " + hours + "h";
    if (hours > 0) return hours + "h " + minutes + "m";
    if (minutes > 0) return minutes + "m";
    return secs + "s";
  }

  function clockLabel(iso) {
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return "—";
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }

  /** "just now" / "4m ago" / "3h ago" — used by the stale + updated lines. */
  function ageLabel(iso) {
    const t = Date.parse(iso);
    if (!Number.isFinite(t)) return "";
    const delta = Date.now() - t;
    if (delta < 45_000) return "just now";
    return humanize(delta) + " ago";
  }

  /* ---------- account masking ------------------------------------------ */

  const UUID_RE =
    /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

  /** Abbreviate account identifiers so no raw e-mail, UUID or long opaque id
   *  ever lands in the popover — or in a screenshot of it. The full value stays
   *  reachable as the element's `title`, which is the only place it may appear.
   *
   *  Rules, in order:
   *    - e-mail (`local@domain`) → at most the first 8 characters of the local
   *      part, then `@…`   (`user@…`). This also covers a value the backend
   *      already masked (`user@…`, `st***@gmail.com`), so double-masking is
   *      idempotent.
   *    - a value the backend already masked (`sk-or-…9f2c`, `…`) passes through.
   *    - a UUID, or any whitespace-free token of 24+ chars, → first 8 + `…`.
   *    - anything short and human (`Plus`, `Team`) passes through untouched. */
  function maskAccount(value) {
    const text = String(value == null ? "" : value).trim();
    if (!text) return "";
    const at = text.indexOf("@");
    if (at !== -1) {
      const head = text.slice(0, at).slice(0, 8);
      return head ? head + "@\u2026" : "\u2026";
    }
    if (text.indexOf("\u2026") !== -1) return text;
    if (UUID_RE.test(text)) return text.slice(0, 8) + "\u2026";
    if (text.length >= 24 && !/\s/.test(text)) return text.slice(0, 8) + "\u2026";
    return text;
  }

  /* ---------- data-source labels ---------------------------------------- */

  /* The backend spells sources camelCase (`oAuth`, `apiKey`) while the mock
   * fixture spells them lower (`mock`). One normalised key drives both the
   * badge text and its CSS class, so `oAuth` and `oauth` can never render as
   * two different things. */
  function sourceKey(source) {
    return String(source == null ? "" : source).toLowerCase();
  }

  const SOURCE_LABELS = {
    mock: "MOCK",
    oauth: "OAUTH",
    apikey: "APIKEY",
    web: "WEB",
    cli: "CLI",
  };

  function sourceLabel(source) {
    const key = sourceKey(source);
    return SOURCE_LABELS[key] || (key ? key.toUpperCase() : "UNKNOWN");
  }

  /* ---------- headline resolution -------------------------------------- */

  function isUsable(w) {
    return w && w.usageKnown !== false && !(w.window && w.window.isSyntheticPlaceholder);
  }

  /** The lane that represents the provider, matching `headline_window()` in Rust. */
  function headlineWindow(snapshot) {
    const order = ["session", "weekly", "weeklyScoped", "extra"];
    for (const kind of order) {
      const candidates = (snapshot.windows || []).filter(
        (w) => w.kind === kind && isUsable(w)
      );
      if (candidates.length) {
        return candidates.reduce((a, b) =>
          b.window.usedPercent > a.window.usedPercent ? b : a
        );
      }
    }
    return null;
  }

  /** Headline usage percent, clamped for display, or null when there is none. */
  function headline(snapshot) {
    const win = headlineWindow(snapshot);
    if (!win) return null;
    return clampPct(win.window.usedPercent);
  }

  /* ---------- reset copy ------------------------------------------------ */

  /** True when the window rolls over soon enough to deserve emphasis. */
  function isImminentReset(win, now) {
    if (!win || !win.resetsAt || !win.windowMinutes) return false;
    const left = Date.parse(win.resetsAt) - (now || Date.now());
    if (!(left > 0)) return false;
    return left / (win.windowMinutes * 60_000) < 0.15;
  }

  /** `resets in 3h 12m` / `reset due · 14:05` / provider prose. */
  function resetText(win, now) {
    if (!win) return "";
    const nowMs = now || Date.now();
    if (win.resetsAt) {
      const left = Date.parse(win.resetsAt) - nowMs;
      if (left > 0) return "resets in " + humanize(left);
      return "reset due · " + clockLabel(win.resetsAt);
    }
    if (win.resetDescription) return win.resetDescription;
    return "no reset info";
  }

  /* ---------- card state classification --------------------------------- */

  /* Claude multi-account / claude-swap sentinels (see repo/docs/claude.md):
   * `token_expired`, `no_credentials`, `keychain_unavailable`, `api_key`.
   * They are not part of the frozen FetchStatus enum, so the UI infers them
   * from the status value when present and from the error prose otherwise. */
  const TOKEN_RE =
    /token[^.]{0,40}(expir|invalid|revoke)|(expir|invalid|revoke)[a-z]*[^.]{0,20}token|re-?auth|sign in again|unauthoriz|401\b|oauth[^.]{0,20}expir/i;

  const STATES = {
    healthy: { code: "healthy", tone: "healthy", label: "Healthy" },
    warning: { code: "warning", tone: "warning", label: "Warning" },
    critical: { code: "critical", tone: "critical", label: "Critical" },
    stale: { code: "stale", tone: "neutral", label: "Stale data" },
    unconfigured: { code: "unconfigured", tone: "neutral", label: "Not set up" },
    error: { code: "error", tone: "critical", label: "Fetch failed" },
    tokenExpired: { code: "tokenExpired", tone: "warning", label: "Token expired" },
    empty: { code: "empty", tone: "neutral", label: "No usage reported" },
  };

  /** Resolve one provider snapshot onto exactly one rendered card state. */
  function classify(snapshot) {
    const status = (snapshot && snapshot.status) || "ok";
    const error = (snapshot && snapshot.error) || "";

    if (status === "tokenExpired" || status === "token_expired" || status === "expired") {
      return STATES.tokenExpired;
    }
    if (status === "noCredentials" || status === "no_credentials" || status === "notConfigured") {
      return STATES.unconfigured;
    }
    if (status === "keychainUnavailable") return STATES.tokenExpired;
    if (status === "error" && TOKEN_RE.test(error)) return STATES.tokenExpired;
    if (status === "error") return STATES.error;
    if (status === "stale") return STATES.stale;

    const used = headline(snapshot);
    if (used === null) return STATES.empty;
    return STATES[severityFor(used)] || STATES.healthy;
  }

  /* ---------- UI-only preferences --------------------------------------- */

  /** The Rust `Settings` payload owns everything the tray app acts on. Two
   *  presentation prefs have no backend field yet, so they live here: mirrored
   *  to localStorage and read by whoever renders the popover. */
  const UI_PREF_KEYS = ["showLegend"];

  const uiPrefs = {
    base: "codexbar.ui.",
    get(key, fallback) {
      try {
        const raw = window.localStorage.getItem(this.base + key);
        return raw === null ? fallback : JSON.parse(raw);
      } catch (err) {
        return fallback;
      }
    },
    set(key, value) {
      try {
        window.localStorage.setItem(this.base + key, JSON.stringify(value));
      } catch (err) {
        /* private mode / no storage: the pref simply does not stick */
      }
    },
    all() {
      const out = {};
      UI_PREF_KEYS.forEach((k) => {
        out[k] = this.get(k, true);
      });
      return out;
    },
  };

  window.CodexBarFormat = {
    SEVERITY_COPY,
    STATES,
    UI_PREF_KEYS,
    ageLabel,
    clampPct,
    classify,
    clockLabel,
    headline,
    headlineWindow,
    humanize,
    isImminentReset,
    isUsable,
    maskAccount,
    pct,
    resetText,
    severityFor,
    sourceKey,
    sourceLabel,
    uiPrefs,
  };
})();
