/* CodexBar for Windows — Settings › Appearance panel.
 *
 * Owns only the #pane-appearance section of settings.html. Everything is
 * persisted through window.CodexBarTheme.save() (ui/theme.js) → the
 * `set_appearance` command → %APPDATA%\CodexBar\appearance.json, which then
 * broadcasts `appearance-changed` so the popover (and the widget) repaint live.
 * In a plain browser (evidence renders) the change is applied locally only.
 */
(function () {
  "use strict";

  const T = window.CodexBarTheme;
  const pane = document.getElementById("pane-appearance");
  if (!T || !pane) return;

  const $ = (id) => document.getElementById(id);
  const els = {
    theme: $("ap-theme"),
    density: $("ap-density"),
    swatches: $("ap-swatches"),
    picker: $("ap-accent-picker"),
    hex: $("ap-accent-hex"),
    contrast: $("ap-contrast"),
    scale: $("ap-font-scale"),
    scaleOut: $("ap-font-scale-out"),
    popover: $("ap-popover-opacity"),
    popoverOut: $("ap-popover-opacity-out"),
    widget: $("ap-widget-opacity"),
    widgetOut: $("ap-widget-opacity-out"),
    legend: $("ap-showLegend"),
    reset: $("ap-reset"),
    toast: $("toast"),
  };

  let toastTimer = null;
  function toast(message) {
    if (!els.toast || !message) return;
    els.toast.textContent = message;
    els.toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => {
      els.toast.hidden = true;
    }, 2400);
  }

  /* ---------- build the swatches once ---------------------------------- */

  T.PRESETS.forEach((preset) => {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "swatch";
    btn.dataset.hex = preset.hex;
    btn.title = preset.label + " " + preset.hex;
    btn.setAttribute("aria-label", preset.label + " accent");
    btn.setAttribute("role", "radio");
    btn.style.setProperty("--swatch", preset.hex);
    btn.addEventListener("click", () => commit({ accent: preset.hex }));
    els.swatches.appendChild(btn);
  });

  /* ---------- paint ------------------------------------------------------ */

  function paintSegmented(group, value) {
    if (!group) return;
    group.querySelectorAll("button[data-value]").forEach((btn) => {
      btn.setAttribute("aria-checked", String(btn.dataset.value === value));
    });
  }

  function pctLabel(value) {
    return Math.round(value * 100) + "%";
  }

  /** Paint the filled part of a range track (accent up to the thumb). */
  function paintFill(input) {
    const min = Number(input.min) || 0;
    const max = Number(input.max) || 100;
    const pct = max > min ? ((Number(input.value) - min) / (max - min)) * 100 : 0;
    input.style.setProperty("--fill", Math.max(0, Math.min(100, pct)) + "%");
  }

  function paint(a) {
    paintSegmented(els.theme, a.theme);
    paintSegmented(els.density, a.density);
    let presetMatch = false;
    els.swatches.querySelectorAll(".swatch").forEach((btn) => {
      const on = btn.dataset.hex === a.accent;
      presetMatch = presetMatch || on;
      btn.setAttribute("aria-checked", String(on));
    });
    if (els.picker) {
      els.picker.value = a.accent;
      // Only the picker carries the "selected" ring when no preset matches,
      // so a preset accent is never shown as selected twice.
      els.picker.dataset.selected = String(!presetMatch);
    }
    if (els.hex && document.activeElement !== els.hex) els.hex.value = a.accent;
    if (els.contrast) {
      const on = T.textOn(a.accent);
      const ratio = T.contrast(on, a.accent);
      const grade = ratio >= 7 ? "AAA" : ratio >= 4.5 ? "AA" : "below AA";
      els.contrast.textContent = "Text on accent " + ratio.toFixed(1) + ":1 · " + grade;
    }
    els.scale.value = String(a.fontScale);
    els.scaleOut.textContent = a.fontScale + "%";
    els.popover.value = String(Math.round(a.popoverOpacity * 100));
    els.popoverOut.textContent = pctLabel(a.popoverOpacity);
    els.widget.value = String(Math.round(a.widgetOpacity * 100));
    els.widgetOut.textContent = pctLabel(a.widgetOpacity);
    [els.scale, els.popover, els.widget].forEach(paintFill);
    if (els.legend) els.legend.setAttribute("aria-checked", String(!!a.showLegend));
  }

  /* ---------- write ------------------------------------------------------ */

  let pending = null;
  let timer = null;

  function commit(patch, delay) {
    // Instant local feedback; the backend echo (and the event) follow.
    T.apply(Object.assign({}, T.current(), pending || {}, patch));
    pending = Object.assign({}, pending || {}, patch);
    clearTimeout(timer);
    timer = setTimeout(flush, delay || 0);
  }

  async function flush() {
    const patch = pending;
    pending = null;
    if (!patch) return;
    try {
      await T.save(patch);
    } catch (err) {
      toast("Could not save appearance: " + (err && err.message ? err.message : err));
    }
  }

  function bindSegmented(group, key) {
    if (!group) return;
    group.addEventListener("click", (event) => {
      const btn = event.target.closest("button[data-value]");
      if (btn) commit({ [key]: btn.dataset.value });
    });
  }

  bindSegmented(els.theme, "theme");
  bindSegmented(els.density, "density");

  els.picker.addEventListener("input", () => commit({ accent: els.picker.value }, 120));
  els.hex.addEventListener("change", () => {
    const hex = T.normalizeHex(els.hex.value);
    if (hex) commit({ accent: hex });
    else {
      toast("Use a hex colour such as #6ea8fe");
      els.hex.value = T.current().accent;
    }
  });

  [els.scale, els.popover, els.widget].forEach((input) =>
    input.addEventListener("input", () => paintFill(input))
  );
  els.scale.addEventListener("input", () => commit({ fontScale: Number(els.scale.value) }, 150));
  els.popover.addEventListener("input", () =>
    commit({ popoverOpacity: Number(els.popover.value) / 100 }, 150)
  );
  els.widget.addEventListener("input", () =>
    commit({ widgetOpacity: Number(els.widget.value) / 100 }, 150)
  );
  if (els.legend) {
    els.legend.addEventListener("click", () =>
      commit({ showLegend: els.legend.getAttribute("aria-checked") !== "true" })
    );
  }

  els.reset.addEventListener("click", () => {
    commit(Object.assign({}, T.DEFAULTS));
    toast("Appearance reset to defaults");
  });

  T.onChange(paint);
  paint(T.current());
})();
