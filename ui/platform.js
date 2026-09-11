/* CodexBar for Windows — platform resolution for keyboard-shortcut labels.
 *
 * Loaded by the popover (index.html), the settings window (settings.html) and
 * the evidence harness, so every surface spells the modifier the same way.
 *
 * Why this file exists, and why it does NOT sniff the browser:
 *   This port ships on Windows only, so the *default* platform is Windows and
 *   the popover always prints `Ctrl R` / `Ctrl ,` / `Ctrl Q`. A static render
 *   (Edge `--headless=new` over `file://`) must be byte-reproducible on any
 *   host, so relying on `navigator.platform` / `userAgent` is exactly what we
 *   avoid: a headless browser can present a macOS-like UA and silently flip the
 *   labels to the Command glyph (U+2318), which is what put macOS shortcuts
 *   into a Windows-port screenshot once. Instead the platform is resolved from
 *   an explicit, documented override and otherwise falls back to Windows.
 *
 * Override (query string, evidence renders only):
 *   ?platform=windows   pin the Windows labels  (`Ctrl R`, `Ctrl ,`, `Ctrl Q`)
 *   ?platform=mac       pin the macOS labels    (U+2318 + key, e.g. `\u2318R`)
 *                       — used only to diff this port against the original
 *                       macOS app
 *   Anything else (or absent) → `windows`, because that is the shipped target.
 *
 * Declare a shortcut in HTML with `data-shortcut="R"` (or `,` / `Q`); this file
 * fills the element's text. The HTML already carries the Windows label as its
 * default, so a page whose script never runs still renders `Ctrl R`.
 */
(function () {
  "use strict";

  /** Modifier label per platform. Windows keeps a space (`Ctrl R`); macOS uses
   *  the stacked Command glyph with no space (`\u2318R`), matching the original
   *  app. The glyph is written as an escape so no raw macOS character ever sits
   *  in a source file of a Windows port. */
  const PLATFORMS = {
    windows: { modifier: "Ctrl", join: " " },
    mac: { modifier: "\u2318", join: "" },
  };

  /** The only supported default — see the header comment. */
  const DEFAULT_PLATFORM = "windows";

  const ALIASES = {
    windows: "windows",
    win: "windows",
    win32: "windows",
    pc: "windows",
    mac: "mac",
    macos: "mac",
    darwin: "mac",
    osx: "mac",
  };

  /** Read `?platform=` without trusting anything else about the host. */
  function platformFromQuery(search) {
    let raw = "";
    try {
      raw = new URLSearchParams(search || "").get("platform") || "";
    } catch (err) {
      raw = "";
    }
    return ALIASES[String(raw).trim().toLowerCase()] || DEFAULT_PLATFORM;
  }

  const current = platformFromQuery(
    typeof window !== "undefined" ? window.location.search : ""
  );

  /** `resolve()` → "windows" | "mac". */
  function resolve() {
    return current;
  }

  /** `shortcut("R")` → "Ctrl R" (Windows) or "\u2318R" (macOS). */
  function shortcut(key) {
    const spec = PLATFORMS[current] || PLATFORMS[DEFAULT_PLATFORM];
    return spec.modifier + spec.join + String(key == null ? "" : key);
  }

  /** Fill every `[data-shortcut]` under `root` (defaults to the document). */
  function apply(root) {
    const scope = root || (typeof document !== "undefined" ? document : null);
    if (!scope || !scope.querySelectorAll) return 0;
    const nodes = scope.querySelectorAll("[data-shortcut]");
    let n = 0;
    for (const node of nodes) {
      node.textContent = shortcut(node.getAttribute("data-shortcut"));
      n += 1;
    }
    return n;
  }

  const api = { DEFAULT_PLATFORM, PLATFORMS, apply, resolve, shortcut };

  if (typeof window !== "undefined") {
    window.CodexBarPlatform = api;
    if (typeof document !== "undefined") {
      const run = () => apply(document);
      if (document.readyState === "loading") {
        document.addEventListener("DOMContentLoaded", run);
      } else {
        run();
      }
    }
  }

  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
})();
