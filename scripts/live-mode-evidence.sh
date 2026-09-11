#!/usr/bin/env bash
# Round-2 evidence: the tray app serving the LIVE registry on this machine.
#
#   bash scripts/live-mode-evidence.sh
#
# Needs a real desktop session (the captures are of real windows) and a build of
# `codexbar-win` in the shared target dir. Writes **new** files only — the
# round-1 artifacts in evidence/ are never touched:
#
#   v2-live-report.json / .txt      the exact payload the popover rendered
#   v2-mock-report.json / .txt      the same command with --mock (sample data)
#   v2-live-popover.png             the real flyout, live mode
#   v2-live-tray-flyout.png         the notification-area overflow flyout
#   v2-live-tray-flyout-uia.json    icon names (= the tooltips, with sources)
#   v2-live-tray-menu.png           the tray context menu (states + sources)
#   v2-live-tray-click-result.png   full screen after clicking a CodexBar icon
#
# Nothing here prints a credential: `--dump-report` writes the same masked
# payload the UI gets.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1
mkdir -p evidence
ROOT_NATIVE="$(cygpath -m "$ROOT" 2>/dev/null || echo "$ROOT")"

TARGET_DIR="${CARGO_TARGET_DIR:-target}"
case "$TARGET_DIR" in
  /*|[A-Za-z]:[\\/]*) ;;
  *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac
APP="$TARGET_DIR/debug/codexbar-win.exe"
[ -f "$APP" ] || { echo "!! $APP missing — run: cargo build -p codexbar-win"; exit 1; }

# `--dump-report` needs forward-slash native paths.
APP_NATIVE="$(cygpath -m "$APP" 2>/dev/null || echo "$APP")"
TMP="$(cygpath -m "${LOCALAPPDATA:-/tmp}/Temp/codexbar-live-evidence" 2>/dev/null || echo /tmp/codexbar-live-evidence)"
rm -rf "$TMP"; mkdir -p "$TMP"

# No stray instance: two tray apps fight over the same icon ids.
powershell -NoProfile -Command "Get-Process codexbar-win -ErrorAction SilentlyContinue | Stop-Process -Force" >/dev/null 2>&1

echo "== 1/4  live report (machine-readable states)"
"$APP" --dump-report "evidence/v2-live-report.json" --live | tee evidence/v2-live-report.txt

echo "== 2/4  mock report (must be sample data only, offline)"
"$APP" --dump-report "evidence/v2-mock-report.json" --mock | tee evidence/v2-mock-report.txt

echo "== 3/4  tray app in live mode -> popover capture"
"$APP" --show >/dev/null 2>&1 &
APP_PID=$!
sleep 8
powershell -ExecutionPolicy Bypass -File scripts/capture.ps1 \
    -ProcessName codexbar-win -TitleMatch 'CodexBar' -OutDir "$TMP" -WaitSeconds 15 || true
# capture.ps1 slugs the window title, so the file names are stable.
[ -f "$TMP/codexbar.png" ] && cp "$TMP/codexbar.png" evidence/v2-live-popover.png \
    && echo "   wrote evidence/v2-live-popover.png"
[ -f "$TMP/codexbar-settings.png" ] && cp "$TMP/codexbar-settings.png" evidence/v2-live-popover-settings.png \
    && echo "   wrote evidence/v2-live-popover-settings.png"

echo "== 4/4  notification area -> tray flyout + context menu"
powershell -ExecutionPolicy Bypass -File scripts/tray-visibility.ps1 -OutDir "$TMP" || true
powershell -ExecutionPolicy Bypass -File scripts/tray-menu.ps1 -OutDir "$TMP" || true

for pair in \
    "tray-flyout.png:v2-live-tray-flyout.png" \
    "tray-flyout-uia.json:v2-live-tray-flyout-uia.json" \
    "tray-click-result.png:v2-live-tray-click-result.png" \
    "tray-menu.png:v2-live-tray-menu.png" \
    "tray-menu-submenu.png:v2-live-tray-menu-submenu.png"
do
    src="${pair%%:*}"; dst="${pair##*:}"
    [ -f "$TMP/$src" ] && cp "$TMP/$src" "evidence/$dst" && echo "   wrote evidence/$dst"
done

kill "$APP_PID" 2>/dev/null
powershell -NoProfile -Command "Get-Process codexbar-win -ErrorAction SilentlyContinue | Stop-Process -Force" >/dev/null 2>&1

echo
echo "== states captured (from the live report) =="
node -e '
  const r = require("fs").readFileSync("evidence/v2-live-report.json", "utf8");
  const report = JSON.parse(r);
  for (const p of report.providers) {
    const err = (p.error || "").slice(0, 60);
    console.log(`  ${p.provider.padEnd(11)} ${String(p.status).padEnd(14)} ${String(p.source).padEnd(8)} ${err}`);
  }
' 2>/dev/null || true
echo
echo "== tray tooltips captured (icon names = per-provider tooltips) =="
grep -o '"name": "[^"]*"' "$TMP/tray-flyout-uia.json" 2>/dev/null | sort -u || echo "   (no UIA dump)"
