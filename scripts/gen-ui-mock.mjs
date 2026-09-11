// Regenerates `ui/mock.js` from the CLI's JSON output.
//
// Usage:
//   cargo run -p codexbar-cli -- usage --format json > evidence/usage-mock.json
//   node scripts/gen-ui-mock.mjs
//
// Keeping the fixture generated (not hand-written) is what guarantees the static
// UI and the Tauri backend render byte-identical payloads.

import { readFileSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const source = join(root, "evidence", "usage-mock.json");
const target = join(root, "ui", "mock.js");

const report = JSON.parse(readFileSync(source, "utf8"));

if (typeof report.schemaVersion !== "number") {
  throw new Error("payload is missing schemaVersion — is it a UsageReport?");
}
if (!Array.isArray(report.providers) || report.providers.length === 0) {
  throw new Error("payload has no providers");
}

const banner =
  "// AUTO-GENERATED fixture — do not edit by hand.\n" +
  "// Regenerate: cargo run -p codexbar-cli -- usage --format json > evidence/usage-mock.json\n" +
  "//             then node scripts/gen-ui-mock.mjs\n" +
  `// Identical to the payload the Tauri backend serves, schemaVersion ${report.schemaVersion}.\n`;

writeFileSync(target, banner + "window.__CODEXBAR_MOCK__ = " + JSON.stringify(report, null, 2) + ";\n");

const states = report.providers.map((p) => `${p.provider}=${p.status}`).join(" ");
console.log(`ui/mock.js updated — schemaVersion ${report.schemaVersion}: ${states}`);
