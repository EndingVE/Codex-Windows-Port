/* CodexBar for Windows — card renderer.
 *
 * One function, `renderCard(snapshot)`, used by the popover and by the states
 * gallery so the two can never drift. It consumes exactly one
 * `ProviderSnapshot` (camelCase, see CONTRACT.md) and knows nothing about where
 * the numbers came from.
 *
 * Card anatomy follows the macOS menu card (repo/docs/screenshots):
 *   [icon] Name                                     account@example.com
 *   updated just now                    PLAN  SOURCE
 *   ────────────────────────────────────────────────────────────
 *   Session · 5h                                            55.9%
 *   ▓▓▓▓▓▓▓▓▓▓░░░░░░░░░░░░░░░░░░░░░░
 *   44.1% left                                  resets in 2h 17m
 */
(function () {
  "use strict";

  const F = window.CodexBarFormat;

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined && text !== null) node.textContent = text;
    return node;
  }

  /** Title-attribute everything that can ellipsize, so truncation is hoverable. */
  function withTooltip(node, text) {
    if (text) node.title = text;
    return node;
  }

  /* ---------- provider mark -------------------------------------------- */

  function logoChip(snapshot) {
    const chip = el("span", "logo-chip");
    const img = el("img");
    img.src = "assets/" + snapshot.provider + ".svg";
    img.alt = "";
    img.onerror = () => {
      img.remove();
      chip.textContent = (snapshot.title || "?").slice(0, 1);
    };
    chip.appendChild(img);
    return chip;
  }

  /* ---------- one quota lane ------------------------------------------- */

  function renderWindowRow(named, index) {
    const win = named.window || {};
    const known = named.usageKnown !== false && !win.isSyntheticPlaceholder;
    const used = F.clampPct(win.usedPercent);
    const severity = F.severityFor(used);

    const row = el("div", "row" + (known ? "" : " is-unknown"));

    const top = el("div", "row-top");
    top.appendChild(withTooltip(el("span", "row-label", named.title), named.title));
    if (known) {
      top.appendChild(
        withTooltip(
          el("span", "row-value " + severity, F.pct(used)),
          F.pct(used) + " used · " + F.SEVERITY_COPY[severity]
        )
      );
    } else {
      top.appendChild(el("span", "row-value unknown", "not reported"));
    }
    row.appendChild(top);

    const bar = el("div", "bar");
    bar.appendChild(el("span", "bar-track"));
    const fill = el("div", "bar-fill " + severity);
    fill.style.width = known ? used + "%" : "0%";
    bar.appendChild(fill);
    if (index === 0 && known && used > 0 && win.windowMinutes) {
      // "Reserve" marker: where the sustainable even-burn rate is right now.
      const elapsed = elapsedFraction(win);
      if (elapsed !== null) {
        const marker = el("span", "bar-pace");
        marker.style.left = (elapsed * 100).toFixed(1) + "%";
        marker.title =
          "Even-burn pace for this window (" + Math.round(elapsed * 100) + "% elapsed)";
        bar.appendChild(marker);
      }
    }
    row.appendChild(bar);

    const foot = el("div", "row-foot");
    foot.appendChild(
      el("span", "row-left", known ? F.pct(Math.max(0, 100 - used)) + " left" : "—")
    );
    const reset = el("span", "reset");
    if (F.isImminentReset(win)) reset.classList.add("soon");
    reset.textContent = known || !win.isSyntheticPlaceholder ? F.resetText(win) : "";
    foot.appendChild(withTooltip(reset, reset.textContent));
    row.appendChild(foot);

    return row;
  }

  function elapsedFraction(win) {
    if (!win.resetsAt || !win.windowMinutes) return null;
    const resetAt = Date.parse(win.resetsAt);
    if (!Number.isFinite(resetAt)) return null;
    const start = resetAt - win.windowMinutes * 60_000;
    const elapsed = (Date.now() - start) / (win.windowMinutes * 60_000);
    if (!(elapsed > 0) || elapsed > 1) return null;
    return elapsed;
  }

  /* ---------- degraded-state notes ------------------------------------- */

  function noteFor(snapshot, state) {
    const error = snapshot.error || "";
    const action = el("button", "action", "Open Settings");
    action.type = "button";
    action.addEventListener("click", () => {
      if (window.__CODEXBAR_OPEN_SETTINGS__) window.__CODEXBAR_OPEN_SETTINGS__();
    });

    if (state.code === "unconfigured") {
      const note = el("div", "card-note setup");
      note.appendChild(el("span", "note-icon", "◆"));
      note.appendChild(
        el("span", "note-text", error || "No credentials configured for this provider.")
      );
      note.appendChild(action);
      return note;
    }

    if (state.code === "tokenExpired") {
      const note = el("div", "card-note token");
      // Warning severity → the amber exclamation, never a neutral glyph.
      note.appendChild(el("span", "note-icon", "!"));
      note.appendChild(
        el(
          "span",
          "note-text",
          error || "Stored credentials expired — sign in again to resume usage."
        )
      );
      note.appendChild(action);
      return note;
    }

    if (state.code === "error") {
      const note = el("div", "card-note error");
      // Critical severity → the cross, matching the red rail + "Fetch failed".
      note.appendChild(el("span", "note-icon", "✕"));
      note.appendChild(el("span", "note-text", error || "Usage fetch failed."));
      return note;
    }

    if (state.code === "stale") {
      const note = el("div", "card-note stale");
      note.appendChild(el("span", "note-icon", "↺"));
      note.appendChild(
        el(
          "span",
          "note-text",
          "Last known values, captured " + (F.ageLabel(snapshot.fetchedAt) || "earlier") + "." +
            (error ? " " + error : "")
        )
      );
      return note;
    }

    if (state.code === "empty") {
      const note = el("div", "card-note empty");
      note.appendChild(el("span", "note-icon", "○"));
      note.appendChild(
        el("span", "note-text", error || "This provider reported no usable quota window.")
      );
      return note;
    }

    return null;
  }

  /* ---------- the card -------------------------------------------------- */

  function renderCard(snapshot) {
    const state = F.classify(snapshot);
    const worst = F.headline(snapshot);
    const card = el("article", "card state-" + state.code);
    card.dataset.provider = snapshot.provider;
    card.dataset.state = state.code;

    // ---- head: icon · name ......................... account (right)
    const head = el("div", "card-head");
    head.appendChild(logoChip(snapshot));
    head.appendChild(withTooltip(el("span", "card-name", snapshot.title), snapshot.title));

    const identity = [];
    if (snapshot.account) identity.push(snapshot.account);
    if (snapshot.plan) identity.push(snapshot.plan);
    if (snapshot.account) {
      // Account only: the plan already has its own badge, so repeating it here
      // is what forced the email to ellipsize. The tooltip keeps both — and is
      // the only surface where the unmasked account may appear.
      const account = withTooltip(
        el("span", "card-account", F.maskAccount(snapshot.account)),
        identity.join(" · ")
      );
      head.appendChild(account);
    }
    card.appendChild(head);

    // ---- meta: updated … ........................ badges
    const meta = el("div", "card-meta");
    const updated = el(
      "span",
      "card-updated",
      state.code === "error" || state.code === "unconfigured"
        ? state.label
        : "updated " + (F.ageLabel(snapshot.fetchedAt) || "—")
    );
    if (state.code !== "healthy") updated.classList.add("is-" + state.tone);
    meta.appendChild(updated);

    const badges = el("div", "card-badges");
    if (worst !== null && state.tone !== "neutral") {
      badges.appendChild(
        withTooltip(
          el("span", "headline " + state.code, F.pct(worst)),
          F.pct(worst) + " used (worst window)"
        )
      );
    }
    if (snapshot.plan) {
      badges.appendChild(withTooltip(el("span", "badge plan", snapshot.plan), "Plan: " + snapshot.plan));
    }
    badges.appendChild(
      withTooltip(
        el("span", "badge source-" + F.sourceKey(snapshot.source), F.sourceLabel(snapshot.source)),
        sourceTooltip(snapshot.source)
      )
    );
    meta.appendChild(badges);
    card.appendChild(meta);

    // ---- quota rows -----------------------------------------------------
    // `usageKnown: false` lanes are kept (CONTRACT.md: "render the label and
    // reset, not a percentage"); only synthetic placeholder lanes are dropped.
    const windows = (snapshot.windows || []).filter(
      (w) => !(w.window && w.window.isSyntheticPlaceholder)
    );
    if (windows.length) {
      const rows = el("div", "rows");
      windows.forEach((w, i) => rows.appendChild(renderWindowRow(w, i)));
      card.appendChild(rows);
    }

    // ---- credit balance (OpenRouter-style) ------------------------------
    if (snapshot.balance) {
      const bal = el("div", "balance");
      const amount = Number(snapshot.balance.amount || 0).toFixed(2);
      const symbol = snapshot.balance.currency === "USD" ? "$" : "";
      bal.appendChild(el("strong", null, symbol + amount + " " + snapshot.balance.currency));
      if (snapshot.balance.label) bal.appendChild(el("span", null, snapshot.balance.label));
      card.appendChild(bal);
    }

    const note = noteFor(snapshot, state);
    if (note) card.appendChild(note);

    return card;
  }

  function sourceTooltip(source) {
    switch (F.sourceKey(source)) {
      case "mock": return "Sample data — nothing left this machine.";
      case "oauth": return "Read from the provider's OAuth credentials.";
      case "apikey": return "Read with an API key.";
      case "cli": return "Scraped from the provider's command-line tool.";
      case "web": return "Read from the provider's web dashboard (browser cookies).";
      default: return String(source == null ? "" : source);
    }
  }

  /* ---------- legend ---------------------------------------------------- */

  /** Discreet key for the bar/percent colours. Text-only + dots, no plates. */
  function renderLegend() {
    const legend = el("div", "legend");
    legend.setAttribute("role", "note");
    legend.setAttribute(
      "aria-label",
      "Bar colours: green under 70 percent used, amber 70 to 89, red 90 or more"
    );
    [["healthy", "healthy"], ["warning", "warning"], ["critical", "critical"]].forEach(
      ([tone, label]) => {
        const item = el("span", "legend-item");
        item.appendChild(el("span", "legend-dot " + tone));
        item.appendChild(el("span", "legend-text", label));
        item.title = label + " — " + F.SEVERITY_COPY[tone];
        legend.appendChild(item);
      }
    );
    legend.appendChild(el("span", "legend-note", "by used %"));
    return legend;
  }

  window.CodexBarRender = { el, renderCard, renderLegend, withTooltip };
})();
