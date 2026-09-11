#!/usr/bin/env bash
# Reproduces every piece of evidence for this build, from a clean checkout.
#
#   bash scripts/verify.sh                 # deterministic gate (default)
#   CODEXBAR_LIVE_CAPTURE=1 bash scripts/verify.sh   # + live window captures
#
# Runs: format check → clippy → workspace tests → CLI build → mock + live CLI
# evidence → static UI renders (Edge headless) → tray app build → evidence
# hygiene gate.
# Native tools get forward-slash `C:/...` paths — MSYS does not translate them.
#
# Everything the script writes lands in `evidence/`.
#
# * Steps 1-6 and 7 (static renders) are deterministic and offline. The `--live`
#   step reads whatever credentials this machine has (env vars /
#   %APPDATA%\CodexBar\config.json); with none, it makes **no** network calls.
#   Credential values never reach a file: payloads only carry masked keys
#   (`sk-or-1v…9f2c`).
# * The live window/tray captures at the end need a desktop session, so they are
#   opt-in (`CODEXBAR_LIVE_CAPTURE=1`) and never gate the build.
# * Step 9 re-scans every text artifact in `evidence/` for raw identifiers
#   (UUID, real e-mail — masked local parts included — token) and the pre-fix
#   lower-case config path, and checks every PNG against
#   `evidence/capture-manifest.json` (sha256 + `containsPii: false`): the gate
#   fails rather than letting a leaked or un-attested capture ship.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
mkdir -p evidence

# Native programs (Edge, cargo, node, powershell) do not understand MSYS
# `/c/...` paths — MSYS path translation is off here, so hand them
# forward-slash drive paths.
ROOT_NATIVE="$(cygpath -m "$ROOT" 2>/dev/null || echo "$ROOT")"

echo "== 1/9  cargo fmt --all -- --check"
cargo fmt --all -- --check

echo "== 2/9  cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings

echo "== 3/9  cargo test --workspace"
cargo test --workspace

echo "== 4/9  build CLI"
cargo build -p codexbar-cli

# Honour a shared CARGO_TARGET_DIR (this box sets one); falling back to
# `./target` silently runs a *stale* binary when the target dir is elsewhere.
TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in
  /*|[A-Za-z]:[\\/]*) ;;                       # already absolute
  *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac
CLI="$TARGET_DIR/debug/codexbar.exe"

echo "== 5/9  CLI mock evidence -> evidence/ (deterministic, no network)"
"$CLI" usage --format json  > evidence/usage-mock.json
"$CLI" usage --format text  > evidence/usage-text.txt
"$CLI" usage --format toon  > evidence/usage-toon.txt
"$CLI" providers            > evidence/providers.txt
"$CLI" schema               > evidence/schema.json
"$CLI" usage --provider codex --format json > evidence/usage-codex.json
"$CLI" usage --provider opencodego --format json > evidence/usage-opencodego.json
node scripts/gen-ui-mock.mjs
"$CLI" --version > /dev/null   # fail fast if the CLI binary is missing/not executable

if "$CLI" usage --format json --fail-at 90 > /dev/null 2>&1; then
  echo "!! expected --fail-at 90 to exit 3"; exit 1
else
  code=$?
  [ "$code" -eq 3 ] || { echo "!! --fail-at exited $code, expected 3"; exit 1; }
  echo "   --fail-at 90 exits 3 as documented (Cursor's weekly sample sits at 93-98%)"
fi

echo "== 6/9  CLI live smoke -> evidence/usage-live.json"
# `--live` exercises the real registry: all 14 providers have a real fetcher
# (each with offline fixture tests). With no credentials it must still emit a
# complete, valid 14-provider payload and exit 0 — which is the point of the
# smoke: the live path can never leave the CLI unusable.
if "$CLI" usage --live --format json > evidence/usage-live.json; then
  node -e '
    const report = JSON.parse(require("fs").readFileSync("evidence/usage-live.json", "utf8"));
    if (report.schemaVersion !== 1) throw new Error("live payload lost schemaVersion");
    if (report.providers.length !== 14) throw new Error(`live payload has ${report.providers.length} providers, expected 14`);
    const ids = report.providers.map((p) => p.provider);
    const expected = ["codex","claude","cursor","openrouter","copilot","gemini","deepseek","groq","zai","minimax","kimi","elevenlabs","xai","opencodego"];
    if (ids.join(",") !== expected.join(",")) throw new Error(`provider order changed: ${ids.join(",")}`);
    for (const p of report.providers) {
      const account = p.account ?? "";
      // A masked key keeps a prefix + "…" + suffix; a bare long token would not.
      if (/^[A-Za-z0-9_-]{20,}$/.test(account)) throw new Error(`${p.provider}: account looks unmasked`);
    }
    const states = report.providers.map((p) => `${p.provider}=${p.status}`).join(" ");
    console.log(`   live payload: 14 providers in canonical order — ${states}`);
    console.log("   live payload: every account field is masked or empty");
  '
else
  echo "!! the live smoke must exit 0 even with no credentials"; exit 1
fi

echo "== 7/9  static UI renders -> evidence/*.png (Edge headless, optional)"
# NB: this Edge build lays out at a fixed 492 px viewport and crops the capture
# to --window-size, so the width must be 492 or content gets sheared off.
EDGE="C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe"
[ -f "$EDGE" ] || EDGE="C:/Program Files/Microsoft/Edge/Application/msedge.exe"
if [ -f "$EDGE" ]; then
  TMP="$(cygpath -m "${TEMP:-/tmp}" 2>/dev/null || echo /tmp)/edge_headless_codexbar"
  RENDERED=()
  render() { # page, width, height, out
    rm -f "evidence/$4"
    "$EDGE" --headless=new --disable-gpu --no-first-run --user-data-dir="$TMP" \
      --hide-scrollbars --virtual-time-budget=6000 --window-size="$2","$3" \
      --screenshot="$ROOT_NATIVE/evidence/$4" \
      "file:///$ROOT_NATIVE/ui/$1" 2>/dev/null || true
    [ -s "evidence/$4" ] && echo "   wrote evidence/$4" || { echo "   !! render of ui/$1 produced no file"; exit 1; }
    RENDERED+=("evidence/$4")
  }
  # `.popover` pins itself to `max-width: 436px` on the left edge (this Edge
  # build refuses a headless viewport narrower than 492 px while still cropping
  # the capture to --window-size). A 436 px capture is therefore the whole
  # flyout, pixel-exact, with no shearing and no dead column.
  render index.html    436  729 ui-popover-436x729.png
  render index.html    436 1200 ui-mock.png
  render index.html    436 2800 ui-popover-full-436x2800.png
  render states.html   436 1720 ui-states-436x1720.png
  # Settings window, one render per pane (deep-linked; see settings.js#boot).
  for pane in general providers display advanced about; do
    render "settings.html#$pane" 900 900 "ui-settings-$pane-900x900.png"
  done
  # The render bytes are time-dependent (relative "resets in 42m" / "sample HH:MM"
  # text), so their sha256 is re-recorded for exactly the files this step wrote.
  # The gate still fails for a PNG that is not in the manifest, and for any other
  # capture whose hash changed — only these names may be re-attested automatically.
  node scripts/evidence-hygiene.mjs --attest "${RENDERED[@]}"
else
  echo "   Edge not found, skipping"
fi

echo "== 8/9  build tray app"
cargo build -p codexbar-win

if [ "${CODEXBAR_LIVE_CAPTURE:-0}" = "1" ]; then
  echo "== live captures (opt-in): needs a desktop session =="
  "$TARGET_DIR/debug/codexbar-win.exe" --show & APP_PID=$!
  sleep 6
  powershell -ExecutionPolicy Bypass -File scripts/capture.ps1 \
      -ProcessName codexbar-win -TitleMatch 'CodexBar' -OutDir "$ROOT_NATIVE/evidence" || true
  powershell -ExecutionPolicy Bypass -File scripts/tray-visibility.ps1 \
      -OutDir "$ROOT_NATIVE/evidence" || true
  powershell -ExecutionPolicy Bypass -File scripts/settings-uia.ps1 \
      -OutDir "$ROOT_NATIVE/evidence" -DumpOnly || true
  kill "$APP_PID" 2>/dev/null || true
else
  echo "== live captures skipped (set CODEXBAR_LIVE_CAPTURE=1 to run them) =="
fi

echo "== 9/9  evidence hygiene gate (no raw uuid / real email / token / stale config path; every PNG attested PII-free)"
node scripts/evidence-hygiene.mjs
node scripts/evidence-hygiene.mjs --self-test

echo "== done =="
ls -la evidence/
echo
echo "Live window screenshot (app must already be running with --show):"
echo "  $TARGET_DIR/debug/codexbar-win.exe --show &"
echo "  powershell -ExecutionPolicy Bypass -File scripts/screenshot-window.ps1 \\"
echo "      -ProcessName codexbar-win -Out \"$ROOT_NATIVE/evidence/app-live.png\""
