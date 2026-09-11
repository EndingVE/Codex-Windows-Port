// Regenerates the README's static captures from the *mock fixture*, headlessly.
//
//   node scripts/render-docs-images.mjs            # render into docs/images/
//   node scripts/render-docs-images.mjs --check    # verify only, write nothing
//
// Why a script instead of ad-hoc screenshots: the popover prints its keyboard
// shortcuts from `ui/platform.js`, and a headless browser is free to present a
// macOS-like user agent. Pinning `?platform=windows` here makes every capture
// byte-reproducible on any host and guarantees the Command glyph (U+2318) can
// never land in an image for a Windows port.
//
// The renders are taken with Edge `--headless=new` over `file://` — the Tauri
// window is never launched, so no real credentials or accounts are involved:
// the pages fall back to `ui/mock.js` (sample data, no network).

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const checkOnly = process.argv.includes("--check");

const COMMAND_GLYPH = "\u2318";
const MAX_BYTES = 60 * 1024; // the README hero must stay small

/** Every capture is pinned to Windows — see the header. */
const PIN = "?platform=windows";

const PAGES = [
  {
    name: "popover.png",
    url: "ui/index.html" + PIN,
    width: 436,
    height: 729,
    label: "README hero — popover with Ctrl shortcuts",
  },
  {
    name: "settings.png",
    url: "ui/settings.html" + PIN,
    width: 900,
    height: 900,
    label: "Settings · General",
  },
];

function findEdge() {
  const candidates = [
    process.env.EDGE_PATH,
    "C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe",
    "C:/Program Files/Microsoft/Edge/Application/msedge.exe",
    "/usr/bin/microsoft-edge",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
  ].filter(Boolean);
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  throw new Error("Microsoft Edge not found; set EDGE_PATH to the binary");
}

const EDGE = findEdge();

function fileUrl(relative) {
  // Native, forward-slash path — the shell/MSYS layer must not rewrite it.
  const native = join(root, relative).split("\\").join("/");
  return "file:///" + native;
}

function edgeBase() {
  return [
    "--headless=new",
    "--disable-gpu",
    "--hide-scrollbars",
    "--force-device-scale-factor=1",
    "--virtual-time-budget=5000",
    "--no-first-run",
    "--no-default-browser-check",
  ];
}

function render(page, outPath) {
  execFileSync(
    EDGE,
    [
      ...edgeBase(),
      `--window-size=${page.width},${page.height}`,
      `--screenshot=${outPath.split("\\").join("/")}`,
      fileUrl(page.url),
    ],
    { stdio: ["ignore", "ignore", "ignore"] }
  );
}

/** Serialised DOM of a page — what the renderer would have pixel-drawn. */
function dom(relative) {
  return execFileSync(
    EDGE,
    [...edgeBase(), "--dump-dom", fileUrl(relative)],
    { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 }
  );
}

/** Load ui/platform.js in a sandbox and read the resolved labels. */
function platformLabels(search) {
  const source = readFileSync(join(root, "ui", "platform.js"), "utf8");
  const sandbox = {
    window: { location: { search } },
    document: { readyState: "complete", querySelectorAll: () => [], addEventListener() {} },
    URLSearchParams,
  };
  sandbox.window.document = sandbox.document;
  vm.createContext(sandbox);
  vm.runInContext(source, sandbox);
  const api = sandbox.window.CodexBarPlatform;
  return {
    platform: api.resolve(),
    refresh: api.shortcut("R"),
    settings: api.shortcut(","),
    quit: api.shortcut("Q"),
  };
}

const failures = [];
function expect(ok, message) {
  if (ok) return;
  failures.push(message);
  console.error("  FAIL " + message);
}

// ---- 1. platform resolution is explicit and defaults to Windows ----------
console.log("platform resolution (ui/platform.js):");
const noOverride = platformLabels("");
expect(noOverride.platform === "windows", "no ?platform= → windows");
expect(noOverride.refresh === "Ctrl R", "no ?platform= → 'Ctrl R'");
expect(noOverride.settings === "Ctrl ,", "no ?platform= → 'Ctrl ,'");

const pinned = platformLabels("?platform=windows");
expect(pinned.refresh === "Ctrl R" && pinned.quit === "Ctrl Q", "?platform=windows → Ctrl labels");

const mac = platformLabels("?platform=mac");
expect(mac.refresh === COMMAND_GLYPH + "R", "?platform=mac -> \u2318R (opt-in diff only)");
console.log(
  `  default=${noOverride.platform} rendered=${noOverride.refresh} / ${noOverride.settings} / ${noOverride.quit};` +
    ` mac(opt-in)=${mac.refresh}`
);

// ---- 2. the DOM the captures are taken from carries no macOS glyph --------
console.log("rendered DOM:");
for (const page of PAGES) {
  const rendered = dom(page.url);
  const shortcuts = [...rendered.matchAll(/data-shortcut="[^"]*"[^>]*>([^<]*)</g)].map((m) => m[1]);
  expect(
    !rendered.includes(COMMAND_GLYPH),
    `${page.url} DOM contains the U+2318 Command glyph`
  );
  console.log(
    `  ${page.url}: ${rendered.includes(COMMAND_GLYPH) ? "U+2318 FOUND" : "no U+2318"}` +
      (shortcuts.length ? ` (shortcuts: ${shortcuts.join(" | ")})` : "")
  );
}
// Sanity: the opt-in macOS render *does* produce the glyph, so the guard above
// is genuinely testing the override rather than grepping an empty page.
expect(
  dom("ui/index.html?platform=mac").includes(COMMAND_GLYPH),
  "?platform=mac DOM should contain U+2318 - the override is not taking effect"
);

if (checkOnly) {
  if (failures.length) {
    console.error(`\n${failures.length} check(s) failed`);
    process.exit(1);
  }
  console.log("\n--check: all platform/shortcut checks passed");
  process.exit(0);
}

// ---- 3. render the captures ----------------------------------------------
const outDir = join(root, "docs", "images");
if (!existsSync(outDir)) mkdirSync(outDir, { recursive: true });

console.log("rendering captures (Edge --headless=new, mock fixture):");
for (const page of PAGES) {
  const outPath = join(outDir, page.name);
  render(page, outPath);
  const bytes = statSync(outPath).size;
  const header = readFileSync(outPath);
  const dims = `${header.readUInt32BE(16)}x${header.readUInt32BE(20)}`;
  expect(bytes > 0, `${page.name} was not written`);
  if (page.name === "popover.png") {
    expect(bytes < MAX_BYTES, `${page.name} is ${bytes} B (budget ${MAX_BYTES} B)`);
  }
  console.log(`  ${page.name} — ${dims}, ${bytes} B — ${page.label}`);
}

if (failures.length) {
  console.error(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("\ndocs/images regenerated with Windows shortcuts");
