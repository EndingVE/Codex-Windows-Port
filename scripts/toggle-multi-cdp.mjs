#!/usr/bin/env node
// Acceptance probe for the settings-window multi-toggle bug (round 7).
//
// Drives the REAL `codexbar-win.exe` settings window over CDP:
//   * launches the binary with **no window flags**, so the settings window is
//     only pre-warmed *hidden* (`settings_window::prewarm`) — the page loads and
//     is fully scriptable while nothing appears on the user's desktop;
//   * `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<CDP_PORT>`;
//   * connects to the settings page target;
//   * clicks the *actual* per-provider toggle buttons in the DOM
//     (`<row .switch>.click()` — exactly what the user clicks), nothing else;
//   * reads the authoritative state back through `window.__TAURI__.core.invoke`
//     ("get_settings") after every click, and the on-disk config.json at the end.
//
// Pre-fix behaviour (reproduced by the orchestrator): off=[codex,cursor,
// openrouter] → click 1 → off grows by one; clicks 2 and 3 are lost, because the
// row handlers kept mutating an `entry` object from the `settings` array that
// the `set_settings` echo had already replaced. Post-fix all three land.
//
// The user's config is NOT left changed by this script: pass the backup path in
// CODEXBAR_CONFIG_BACKUP and it restores + verifies before exiting.
//
// NOTE on the profile: WebView2 ignores `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`
// when another process already owns the same user-data folder — and every build
// of this app shares `%LOCALAPPDATA%\com.codexbar.win\EBWebView`. So the probe
// gives itself an isolated profile via `WEBVIEW2_USER_DATA_FOLDER` and its own
// port (default 9333), which also keeps it off a second instance's 9222.
//
//   node scripts/toggle-multi-cdp.mjs
//
// Prints a single JSON object on stdout. Exit 0 = pass.

import { spawn, spawnSync } from "node:child_process";
import { readFileSync, writeFileSync, existsSync, copyFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

// Resolved relative to this script so the repo stays location-independent.
const SCRIPT_DIR = fileURLToPath(new URL(".", import.meta.url));
const WINAPP_ROOT = join(SCRIPT_DIR, "..");
const REPO_ROOT = join(WINAPP_ROOT, "..");

const EXE = process.env.CODEXBAR_EXE ||
  join(REPO_ROOT, ".cargo-target", "debug", "codexbar-win.exe");
const PORT = Number(process.env.CDP_PORT || 9333);
const UDF = process.env.WEBVIEW2_USER_DATA_FOLDER ||
  join(process.env.LOCALAPPDATA || "", "Temp", "cb-cdp-udf");
const BACKUP = process.env.CODEXBAR_CONFIG_BACKUP ||
  join(process.env.LOCALAPPDATA || "", "Temp", "codexbar-config-backup.json");
const CONFIG = join(process.env.APPDATA || "", "CodexBar", "config.json");
const SHOT = process.env.CODEXBAR_SHOT || join(WINAPP_ROOT, "evidence", "v8-settings-providers-toggles.png");
const PICK_COUNT = 3;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function killApp() {
  spawnSync("taskkill", ["/F", "/IM", "codexbar-win.exe"], { stdio: "ignore" });
}

async function findTarget() {
  const deadline = Date.now() + 40_000;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`http://127.0.0.1:${PORT}/json/list`);
      const list = await res.json();
      const hit = list.find(
        (t) => t.type === "page" && /codexbar-settings|settings\.html/.test(t.url || "")
      );
      if (hit && hit.webSocketDebuggerUrl) return hit;
    } catch {
      /* not up yet */
    }
    await sleep(500);
  }
  throw new Error("settings.html CDP target never appeared");
}

function connect(url) {
  const ws = new WebSocket(url);
  let id = 0;
  const pending = new Map();
  const ready = new Promise((resolve, reject) => {
    ws.addEventListener("open", () => resolve());
    ws.addEventListener("error", (e) => reject(new Error("ws error: " + e.message)));
  });
  ws.addEventListener("message", (ev) => {
    let msg;
    try { msg = JSON.parse(ev.data); } catch { return; }
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(JSON.stringify(msg.error))) : resolve(msg.result);
    }
  });
  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const n = ++id;
      pending.set(n, { resolve, reject });
      ws.send(JSON.stringify({ id: n, method, params }));
    });
  const evaluate = async (expression) => {
    const r = await send("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (r.exceptionDetails) {
      throw new Error("evaluate threw: " + JSON.stringify(r.exceptionDetails.exception?.description || r.exceptionDetails));
    }
    return r.result.value;
  };
  return { ws, ready, send, evaluate };
}

async function waitReady(evaluate) {
  const deadline = Date.now() + 40_000;
  while (Date.now() < deadline) {
    try {
      const ok = await evaluate(
        "!!(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke && document.querySelectorAll('.provider-row[data-provider]').length > 0)"
      );
      if (ok) return;
    } catch { /* page still booting */ }
    await sleep(400);
  }
  throw new Error("settings page never finished booting");
}

const readSettings = (evaluate) =>
  evaluate('window.__TAURI__.core.invoke("get_settings")');

const offOf = (settings) =>
  settings.providers.filter((p) => p.enabled === false).map((p) => p.id).sort();

/** Click the real toggle button for `id`; returns the row label for the log. */
const clickToggle = (evaluate, id) =>
  evaluate(`(() => {
    const row = document.querySelector('.provider-row[data-provider="${id}"]');
    if (!row) return "missing-row";
    const sw = row.querySelector('.switch');
    if (!sw) return "missing-switch";
    const before = sw.getAttribute('aria-checked');
    sw.click();
    return before + "->" + sw.getAttribute('aria-checked');
  })()`);

function diskOff() {
  const raw = JSON.parse(readFileSync(CONFIG, "utf8"));
  return raw.providers.filter((p) => p.enabled === false).map((p) => p.id).sort();
}

async function main() {
  const out = { schema: "codexbar-toggle-multi-cdp/1", steps: [], result: null };
  // Any surviving WebView2 browser process still holding our probe profile would
  // make WebView2 ignore the additional browser arguments (no debug port).
  spawnSync("powershell", ["-NoProfile", "-Command",
    "Get-CimInstance Win32_Process -Filter \"Name='msedgewebview2.exe'\" | " +
    "Where-Object { $_.CommandLine -match 'cb-cdp-udf' } | ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"
  ], { stdio: "ignore" });

  killApp();
  await sleep(1200);
  mkdirSync(UDF, { recursive: true });

  const child = spawn(EXE, (process.env.CODEXBAR_ARGS || "").split(" ").filter(Boolean), {
    env: {
      ...process.env,
      WEBVIEW2_USER_DATA_FOLDER: UDF,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${PORT}`,
    },
    stdio: "ignore",
    detached: false,
  });
  out.exe = EXE;
  out.pid = child.pid;
  out.cdpPort = PORT;

  let client = null;
  try {
    const target = await findTarget();
    out.targetUrl = target.url;
    client = connect(target.webSocketDebuggerUrl);
    await client.ready;
    await client.send("Runtime.enable");
    await client.send("Page.enable");
    await waitReady(client.evaluate);

    // Informational: what the running page actually serves (the behavioural
    // assertions below are the gate — a stale bundle cannot make three
    // consecutive toggles persist).
    out.scriptSources = await client.evaluate(`[...document.scripts].map((s) => s.src)`);
    out.servedScriptHasFix = await client.evaluate(`(async () => {
      try {
        const t = await (await fetch('settings.js')).text();
        return { len: t.length, syncProviderRows: t.includes('syncProviderRows'), adoptRemote: t.includes('adoptRemote') };
      } catch (e) { return { error: String(e) }; }
    })()`);

    // Focus the Providers pane so the screenshot is the one the user sees.
    await client.evaluate(`document.querySelector('.nav-item[data-pane="providers"]').click(), true`);
    await sleep(300);

    const initial = await readSettings(client.evaluate);
    const initialOff = offOf(initial);
    out.initialOff = initialOff;

    // Three DISTINCT providers that were ON at the start.
    const picks = initial.providers.filter((p) => p.enabled !== false).map((p) => p.id).slice(0, PICK_COUNT);
    out.picks = picks;
    if (picks.length < PICK_COUNT) throw new Error("not enough enabled providers to toggle");

    const offAfter = [];
    for (let i = 0; i < picks.length; i++) {
      const id = picks[i];
      const aria = await clickToggle(client.evaluate, id);
      await sleep(900); // let set_settings + the echo land
      const off = offOf(await readSettings(client.evaluate));
      offAfter.push(off);
      out.steps.push({ click: i + 1, provider: id, aria, off });
    }
    // On-disk state right after the three consecutive OFF clicks.
    out.diskOffAfterThreeClicks = diskOff();

    // Turning one back ON must work too (the report is "apagar/encender").
    const reEnable = picks[0];
    const ariaOn = await clickToggle(client.evaluate, reEnable);
    await sleep(900);
    const offAfterReEnable = offOf(await readSettings(client.evaluate));
    out.steps.push({ click: 4, provider: reEnable, aria: ariaOn, off: offAfterReEnable, kind: "enable" });
    out.diskOffAfterReEnable = diskOff();

    // Screenshot of the Providers pane with the toggles as they now stand.
    await client.evaluate(`document.querySelector('.nav-item[data-pane="providers"]').click(), true`);
    await sleep(400);
    const shot = await client.send("Page.captureScreenshot", { format: "png", fromSurface: true });
    writeFileSync(SHOT, Buffer.from(shot.data, "base64"));
    out.screenshot = SHOT;

    const lastOff = offAfter[offAfter.length - 1];
    const expectOff = [...new Set([...initialOff, ...picks])].sort();
    const allThreeStuck = picks.every((id) => lastOff.includes(id));
    const diskMatches = picks.every((id) => out.diskOffAfterThreeClicks.includes(id));
    const reEnabled = !offAfterReEnable.includes(reEnable);
    const reEnabledOnDisk = !out.diskOffAfterReEnable.includes(reEnable);

    out.result = {
      expectOffAfterThreeClicks: expectOff,
      offAfterThreeClicks: lastOff,
      allThreePersistedInApp: allThreeStuck,
      allThreePersistedOnDisk: diskMatches,
      reEnableWorkedInApp: reEnabled,
      reEnableWorkedOnDisk: reEnabledOnDisk,
      pass: allThreeStuck && diskMatches && reEnabled && reEnabledOnDisk,
    };
  } finally {
    try { client?.ws.close(); } catch {}
    child.kill();
    killApp();
  }

  // Restore the user's config, then prove it went back.
  if (existsSync(BACKUP)) {
    copyFileSync(BACKUP, CONFIG);
    out.restored = { from: BACKUP, offNow: diskOff() };
    out.result.restoredToOriginal =
      JSON.stringify(out.restored.offNow) === JSON.stringify(["codex", "cursor", "openrouter"]);
    out.result.pass = out.result.pass && out.result.restoredToOriginal;
  } else {
    out.restored = null;
    out.result.pass = false;
  }

  console.log(JSON.stringify(out, null, 2));
  process.exit(out.result.pass ? 0 : 1);
}

main().catch((err) => {
  killApp();
  console.log(JSON.stringify({ schema: "codexbar-toggle-multi-cdp/1", error: String(err && err.stack || err) }, null, 2));
  process.exit(2);
});
