#!/usr/bin/env node
// Evidence hygiene gate — fails if any artifact under `evidence/` carries a raw
// secret / real-domain e-mail address, or if any PNG is not attested PII-free.
//
// The reviewer's findings that prompted this:
//   * round 2/3 reports published a raw account UUID and a real e-mail address;
//   * round 4 shipped `v4-app-popover.png` with the *full* address painted in
//     the Codex card, and three notes/JSON files that carried a **masked local
//     part with the real domain** (`st***@gmail.com`) — which the first version
//     of this gate did not catch, because its e-mail rule demanded ordinary
//     address characters before the `@`, and because PNGs were never inspected.
//
// So the gate now has three jobs:
//   1. text rules, including masked-local-part addresses with a non-documentation
//      domain (`st***@gmail.com`, `…@empresa.com`, `anything@real.tld`);
//   2. PNG coverage: every PNG under `evidence/` must be listed in
//      `evidence/capture-manifest.json` with `containsPii: false` and a matching
//      sha256 — a new capture cannot be added without an explicit attestation,
//      and a changed capture fails until it is re-attested by name;
//   3. `--self-test` proves all of the above by planting synthetic probes.
//
//   node scripts/evidence-hygiene.mjs                  # scan, exit 1 on a hit
//   node scripts/evidence-hygiene.mjs --self-test      # prove the scan catches
//                                                      #   a UUID, a masked
//                                                      #   address, an unlisted
//                                                      #   PNG and a stale hash
//   node scripts/evidence-hygiene.mjs --attest <file>… # re-record sha256 of
//                                                      #   manifest entries
//                                                      #   (never adds new ones)
//
// Scanned: every text artifact (json/jsonl/txt/md/html/js/mjs/css/csv/svg/xml/
// yaml/yml/log) plus every PNG (via the manifest). A hit is printed with the
// value masked — the gate never echoes the secret it found.

import { readdirSync, readFileSync, statSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const EVIDENCE = join(ROOT, "evidence");
const MANIFEST = join(EVIDENCE, "capture-manifest.json");

const TEXT_EXT = /\.(json|jsonl|txt|md|html|htm|js|mjs|css|csv|svg|xml|yaml|yml|log)$/i;

// Domains that exist only for documentation; anything else is a real address.
const DOC_DOMAINS = ["example.com", "example.org", "example.net", "example.edu"];
const DOC_SUFFIX = [".example", ".test", ".invalid", ".localhost"];
// A trailing `.<ext>` is a filename (`logo@2x.png`), not a domain.
const FILE_TLD = new Set([
  "png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "css", "js", "mjs",
  "json", "html", "htm", "md", "txt", "yaml", "yml", "toml", "log", "csv", "xml",
  "mp3", "mp4", "wav", "exe", "dll", "ps1", "sh", "lock",
]);

const RULES = [
  {
    name: "raw-uuid",
    // 8-4-4-4-12 hex. A masked id (`abcd1234…`) has no dashes and cannot match.
    re: /\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b/g,
  },
  {
    name: "real-email",
    // An `@` followed by a domain with a real TLD. The local part is
    // deliberately loose: `st***@gmail.com` and `…@empresa.com` leak the real
    // domain just as loudly as `user@empresa.com` does, and the previous
    // `[A-Za-z0-9._%+-]+` class let the first two through.
    re: /[^\s"'`<>()[\]{},;:]*@(?:[A-Za-z0-9-]+\.)+[A-Za-z]{2,}/g,
    allow: (m) => {
      const domain = m.slice(m.lastIndexOf("@") + 1).toLowerCase();
      if (FILE_TLD.has(domain.slice(domain.lastIndexOf(".") + 1))) return true;
      return (
        DOC_DOMAINS.includes(domain) || DOC_SUFFIX.some((suffix) => domain.endsWith(suffix))
      );
    },
  },
  {
    name: "api-key",
    re: /\bsk-[A-Za-z0-9_-]{12,}\b/g,
  },
  {
    name: "jwt",
    re: /\beyJ[A-Za-z0-9_-]{12,}\b/g,
  },
  {
    name: "github-token",
    re: /\bgho_[A-Za-z0-9]{12,}\b/g,
  },
  {
    name: "google-token",
    re: /\bya29\.[A-Za-z0-9_-]{12,}\b/g,
  },
  {
    name: "bearer-value",
    // `Bearer <redacted>` / `Bearer ***` are the documented placeholders.
    re: /Bearer\s+([A-Za-z0-9._-]{12,})/g,
    allow: (m, value) => /^<?(redacted|\*+)>?$/i.test(value),
  },
  {
    name: "refresh-token",
    re: /"refresh_token"\s*:\s*"([^"]{12,})"/g,
    allow: (m, value) => /…|redacted|\.\.\./.test(value),
  },
  {
    name: "stale-config-path",
    // The pre-fix hints spelled the Windows directory in lower case. In JSON the
    // separator is escaped (`%APPDATA%\\codexbar`), so accept one or two.
    re: /%APPDATA%\\+codexbar/g,
    exact: true,
  },
];

/** Mask a hit so the report never repeats the value it is reporting on. */
function mask(value) {
  const text = String(value).trim();
  if (text.length <= 4) return "…";
  return `${text.slice(0, 4)}…`;
}

function* walkText(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walkText(path);
    else if (TEXT_EXT.test(entry.name)) yield path;
  }
}

function* walkPng(dir) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walkPng(path);
    else if (/\.png$/i.test(entry.name)) yield path;
  }
}

/** Every text artifact under evidence/, rule by rule, line by line. */
function scanText() {
  const findings = [];
  for (const file of walkText(EVIDENCE)) {
    let text;
    try {
      text = readFileSync(file, "utf8");
    } catch {
      continue;
    }
    const lines = text.split(/\r?\n/);
    for (const rule of RULES) {
      lines.forEach((line, index) => {
        rule.re.lastIndex = 0;
        let match;
        while ((match = rule.re.exec(line)) !== null) {
          const value = rule.exact ? match[0] : match[rule.allow ? 1 : 0] ?? match[0];
          if (rule.allow && rule.allow(match[0], value)) continue;
          findings.push({
            file: relative(ROOT, file).replace(/\\/g, "/"),
            line: index + 1,
            rule: rule.name,
            sample: mask(value),
          });
          if (!rule.re.global) break;
        }
      });
      rule.re.lastIndex = 0;
    }
  }
  return findings;
}

function sha256(file) {
  return createHash("sha256").update(readFileSync(file)).digest("hex");
}

function loadManifest() {
  if (!existsSync(MANIFEST)) return null;
  try {
    return JSON.parse(readFileSync(MANIFEST, "utf8"));
  } catch (error) {
    return { __error: String(error) };
  }
}

/**
 * PNG coverage. Fails on:
 *   - a PNG under evidence/ missing from the manifest (an un-attested capture);
 *   - a manifest entry that is not `containsPii: false`;
 *   - a manifest entry whose sha256 no longer matches the file on disk;
 *   - a manifest entry whose file no longer exists.
 * `extraEntries` lets the self-test inject a synthetic manifest entry.
 */
function checkManifest(manifest, extraEntries = null) {
  const findings = [];
  const entries = { ...(manifest?.pngs ?? {}), ...(extraEntries ?? {}) };

  if (!manifest) {
    findings.push({ file: "evidence/capture-manifest.json", rule: "png-manifest-missing", sample: "…" });
    return findings;
  }
  if (manifest.__error) {
    findings.push({ file: "evidence/capture-manifest.json", rule: "png-manifest-unreadable", sample: "…" });
    return findings;
  }

  for (const file of walkPng(EVIDENCE)) {
    const rel = relative(ROOT, file).replace(/\\/g, "/");
    const entry = entries[rel];
    if (!entry) {
      findings.push({ file: rel, rule: "png-unattested", sample: "…" });
      continue;
    }
    if (entry.containsPii !== false) {
      findings.push({ file: rel, rule: "png-pii-attested", sample: "…" });
      continue;
    }
    let actual = null;
    try {
      actual = sha256(file);
    } catch {
      findings.push({ file: rel, rule: "png-unreadable", sample: "…" });
      continue;
    }
    if (entry.sha256 !== actual) {
      findings.push({
        file: rel,
        rule: entry.render ? "png-hash-stale-render" : "png-hash-changed",
        sample: `#${String(entry.sha256).slice(0, 6)}…≠#${actual.slice(0, 6)}…`,
      });
    }
  }

  for (const rel of Object.keys(entries)) {
    if (!rel.startsWith("evidence/")) continue;
    if (!existsSync(join(ROOT, rel))) {
      findings.push({ file: rel, rule: "png-manifest-stale-entry", sample: "…" });
    }
  }
  return findings;
}

function scanAll(extraEntries = null) {
  return [...scanText(), ...checkManifest(loadManifest(), extraEntries)];
}

function report(findings) {
  console.error(`!! evidence/ carries ${findings.length} raw value(s) / unattested capture(s):`);
  for (const hit of findings) {
    const where = hit.line ? `${hit.file}:${hit.line}` : hit.file;
    console.error(`   ${where}  [${hit.rule}] ${hit.sample}`);
  }
  console.error("   Mask them at the source (provider snapshot) or regenerate the artifact,");
  console.error("   then re-attest with: node scripts/evidence-hygiene.mjs --attest <file>");
}

/** `--attest <file>…` re-records sha256 for manifest entries that already exist. */
function attest(files) {
  const manifest = loadManifest();
  if (!manifest) {
    console.error("!! evidence/capture-manifest.json is missing — create it first");
    return 1;
  }
  let code = 0;
  for (const arg of files) {
    const rel = relative(ROOT, join(ROOT, arg)).replace(/\\/g, "/");
    const entry = manifest.pngs[rel];
    if (!entry) {
      console.error(
        `!! ${rel}: not in evidence/capture-manifest.json — add it by hand with its` +
          ` containsPii attestation first (an unlisted PNG must never be auto-attested)`
      );
      code = 2;
      continue;
    }
    if (!existsSync(join(ROOT, rel))) {
      console.error(`!! ${rel}: no such file`);
      code = 2;
      continue;
    }
    entry.sha256 = sha256(join(ROOT, rel));
    console.log(`   attested ${rel} #${entry.sha256.slice(0, 8)}…`);
  }
  writeFileSync(MANIFEST, JSON.stringify(manifest, null, 2) + "\n", "utf8");
  return code;
}

function selfTest() {
  const probeText = join(EVIDENCE, ".hygiene-selftest.json");
  const probePng = join(EVIDENCE, ".hygiene-selftest.png");
  const fakeUuid = "0123abcd-4567-89ef-0123-456789abcdef";
  const fakeMaskedMail = "st***@gmail.com";
  writeFileSync(probeText, `{"account": "${fakeUuid}", "masked": "${fakeMaskedMail}"}\n`, "utf8");
  // A minimal, valid-enough PNG; its sha256 will not match the synthetic entry.
  writeFileSync(probePng, Buffer.from("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000a49444154789c6300010000050001", "hex"));
  let code = 0;
  try {
    const textHits = scanText().filter((hit) => hit.file.endsWith(".hygiene-selftest.json"));
    const uuid = textHits.find((hit) => hit.rule === "raw-uuid");
    const mail = textHits.find((hit) => hit.rule === "real-email");
    if (!uuid) {
      console.error("!! self-test: the scan did NOT catch a planted raw UUID");
      code = 1;
    } else {
      console.log(`   self-test: planted UUID caught (${uuid.file}:${uuid.line}, ${uuid.rule})`);
    }
    if (!mail) {
      console.error("!! self-test: the scan did NOT catch a planted masked address (st***@gmail.com)");
      code = 1;
    } else {
      console.log(`   self-test: planted masked address caught (${mail.file}:${mail.line}, ${mail.rule})`);
    }
    // The documented maskings must stay clean, or the gate would be unusable.
    const benign = ["user@…", "user@example.com", "work@example.org", "logo@2x.png"];
    const realEmail = RULES.find((rule) => rule.name === "real-email");
    const noisy = benign.filter((sample) => {
      realEmail.re.lastIndex = 0;
      let m;
      while ((m = realEmail.re.exec(`x ${sample} x`)) !== null) {
        if (!realEmail.allow(m[0], m[0])) return true;
      }
      return false;
    });
    if (noisy.length) {
      console.error(`!! self-test: benign masked forms flagged: ${noisy.join(", ")}`);
      code = 1;
    } else {
      console.log(`   self-test: benign forms left alone (${benign.join(", ")})`);
    }
    // PNG coverage: an unlisted PNG must fail.
    const unlisted = checkManifest(loadManifest()).filter(
      (hit) => hit.file === "evidence/.hygiene-selftest.png" && hit.rule === "png-unattested"
    );
    if (!unlisted.length) {
      console.error("!! self-test: an unlisted PNG under evidence/ was NOT flagged");
      code = 1;
    } else {
      console.log("   self-test: unlisted PNG caught (evidence/.hygiene-selftest.png, png-unattested)");
    }
    // PNG coverage: a listed-but-changed PNG must fail.
    const changed = checkManifest(loadManifest(), {
      "evidence/.hygiene-selftest.png": { sha256: "0".repeat(64), containsPii: false, render: false },
    }).filter((hit) => hit.file === "evidence/.hygiene-selftest.png" && hit.rule === "png-hash-changed");
    if (!changed.length) {
      console.error("!! self-test: a changed PNG hash was NOT flagged");
      code = 1;
    } else {
      console.log("   self-test: changed PNG hash caught (evidence/.hygiene-selftest.png, png-hash-changed)");
    }
    // PNG coverage: an entry attested as containing PII must fail.
    const pii = checkManifest(loadManifest(), {
      "evidence/.hygiene-selftest.png": { sha256: sha256(probePng), containsPii: true, render: false },
    }).filter((hit) => hit.file === "evidence/.hygiene-selftest.png" && hit.rule === "png-pii-attested");
    if (!pii.length) {
      console.error("!! self-test: a PNG attested `containsPii: true` was NOT flagged");
      code = 1;
    } else {
      console.log("   self-test: containsPii:true entry caught (evidence/.hygiene-selftest.png, png-pii-attested)");
    }
    return code;
  } finally {
    rmSync(probeText, { force: true });
    rmSync(probePng, { force: true });
  }
}

const argv = process.argv.slice(2);

if (argv.includes("--self-test")) {
  const code = selfTest();
  for (const probe of [".hygiene-selftest.json", ".hygiene-selftest.png"]) {
    rmSync(join(EVIDENCE, probe), { force: true });
  }
  process.exit(code);
}

const attestAt = argv.indexOf("--attest");
if (attestAt !== -1) {
  process.exit(attest(process.argv.slice(process.argv.indexOf("--attest") + 1)));
}

const findings = scanAll();
if (findings.length === 0) {
  const manifest = loadManifest();
  const count = Object.keys(manifest?.pngs ?? {}).length;
  console.log(
    `   evidence/: no raw uuid, real email, token or stale config path; ${count} PNG(s) listed and attested PII-free`
  );
  process.exit(0);
}
report(findings);
process.exit(1);
