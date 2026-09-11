/* CodexBar for Windows — states gallery.
 *
 * Renders one card per state from `mock-states.js` through the *shipping*
 * renderer (`render.js`), so this page is evidence about the real popover and
 * not a re-implementation. Used to produce evidence/ui-states-*.png.
 */
(function () {
  "use strict";

  const F = window.CodexBarFormat;
  const R = window.CodexBarRender;
  const report = window.__CODEXBAR_STATES__;
  const host = document.getElementById("stateCards");

  const legendHost = document.getElementById("stateLegend");
  if (legendHost) legendHost.replaceWith(R.renderLegend());

  (report.providers || []).forEach((snapshot) => {
    const state = F.classify(snapshot);

    const caption = document.createElement("p");
    caption.className = "gallery-caption";
    caption.dataset.state = state.code;
    caption.textContent = state.code + "  ←  " + snapshot.provider +
      "  (" + (snapshot.status || "ok") + ")";

    const card = R.renderCard(snapshot);
    host.appendChild(caption);
    host.appendChild(card);
  });
})();
