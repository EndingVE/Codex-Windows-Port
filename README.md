# CodexBar for Windows

A **Windows port of [CodexBar](https://github.com/steipete/CodexBar)** — the
macOS tray app that shows AI-provider usage limits at a glance — written from
scratch in **Rust + [Tauri 2](https://tauri.app)**.

It sits in the notification area, draws a small donut gauge (the arc tracks the
provider closest to its limit), and opens a flyout with one card per provider:
session and weekly windows, used percentage, a progress bar, the plan/tier and a
reset countdown. The same data is available from a `codexbar` CLI.

> **Not affiliated with the original project.** This is an independent,
> source-level port of the behaviour and data model of CodexBar (MIT). See
> [License & attribution](#license--attribution).

## Status — read this first

This port is honest about how far along it is:

- ✅ **14 providers have real fetchers** (OAuth file, API key or CLI token
  scrape), each covered by an offline fixture test. The original CodexBar
  supports **~85**; the rest are **not** ported.
- ✅ Mock mode, the CLI, the tray icon, the popover and the settings window work
  on Windows 10/11.
- ❌ **Not included:** cost/history views, notifications, a desktop widget, and
  a packaged installer. `bundle.targets` is configured but has not been
  exercised.
- A provider without local credentials reports `notConfigured` — it never
  invents a number.

**Mock mode is the default.** `codexbar usage` reads no credentials and calls no
endpoints. Only `--live` reads local credentials and hits the providers'
usage endpoints.

## Screenshot

The UI is a close match for the original. Rather than ship stale images here,
see the screenshots in the
[upstream README](https://github.com/steipete/CodexBar) — the popover layout,
cards and bar colours follow the same design.

## Providers (14)

| Provider | Auth | What the port reads |
|---|---|---|
| Codex | local OAuth file | `%USERPROFILE%\.codex\auth.json` (owned by the Codex CLI) |
| Claude | local OAuth file | `%USERPROFILE%\.claude\.credentials.json` (owned by Claude Code) |
| Cursor | local OAuth file | Cursor's VS Code DB `state.vscdb` under `%APPDATA%\Cursor\User\globalStorage\` |
| Gemini | local OAuth file | `%USERPROFILE%\.gemini\oauth_creds.json` (owned by the Gemini CLI) |
| Copilot | device flow | token issued by GitHub's device flow, stored by this app |
| OpenRouter | API key | `OPENROUTER_API_KEY` |
| DeepSeek | API key | `DEEPSEEK_API_KEY` |
| Groq | API key / session | `GROQ_API_KEY` (or a Groq console session token) |
| z.ai | API key | `Z_AI_API_KEY` |
| MiniMax | API key | `MINIMAX_CODING_API_KEY` (or `MINIMAX_API_KEY`) |
| Kimi | API key / CLI token | `KIMI_CODE_API_KEY`, or the read-only token in `%USERPROFILE%\.kimi-code\credentials\kimi-code.json` |
| ElevenLabs | API key | `ELEVENLABS_API_KEY` (or `XI_API_KEY`) |
| xAI | API key | `XAI_MANAGEMENT_API_KEY` |
| OpenCode Go | API key / CLI | `OPENCODE_API_KEY`, or an existing OpenCode login |

`%APPDATA%\CodexBar\config.json` is this app's own settings file.

## Requirements

- **Windows 10 or 11**
- **WebView2 runtime** (preinstalled on Windows 11 and recent Windows 10)
- To build from source: **Rust (MSVC toolchain), stable ≥ 1.77,** and the
  **WebView2** build prerequisites. No Node.js or bundler is required — the
  frontend is plain HTML/CSS/JS embedded at compile time.

## Build

```bash
# from the repository root (the winapp/ workspace)
cargo build --release -p codexbar-win   # -> target/release/codexbar-win.exe  (tray app)
cargo build --release -p codexbar-cli   # -> target/release/codexbar.exe      (CLI)
```

Run the test suite (offline, no network):

```bash
cargo test --workspace
```

For a debug build use `cargo build -p codexbar-win`; the binaries land in
`target/debug/`.

## Use

**Tray app.** Launch `codexbar-win.exe`; it lives in the notification area (no
window unless you open the flyout). Left-click the icon for the usage flyout,
right-click for the menu (refresh now, Settings, Quit). The icon's donut arc
tracks the provider closest to its limit.

`codexbar-win.exe --show` opens the popover immediately — handy for
screenshots and manual checks.

**Settings** let you enable/disable and reorder providers, set the refresh
cadence, toggle start-at-login, pick tray-display options, and trigger
per-provider credential refresh. Saved atomically to
`%APPDATA%\CodexBar\config.json`.

**CLI.**

```
codexbar usage [--format text|json|jsonl|toon] [--provider <id>] [--live] [--fail-at <pct>]
codexbar providers
codexbar schema
```

Example (mock mode, truncated):

```jsonc
// codexbar usage --format json
{
  "generatedAt": "…",
  "providers": [
    { "id": "codex",  "name": "Codex",  "account": "user@…",
      "plan": "Plus", "status": "ok",
      "windows": [ { "kind": "session", "usedPercent": 42, "resetsAt": "…" } ] }
  ]
}
```

`--live` switches from the deterministic mock registry to the real fetchers;
with no credentials it stays offline and reports `notConfigured`. Both modes
emit the **same payload shape**, and credential values never reach stdout —
`account` only ever carries a masked stub. `--fail-at N` exits **3** when any
provider is at or above N % used, so it drops into scripts and status bars
unchanged.

## Security notes

- **Read-only.** The port only ever *reads* existing credentials. It never
  prints, logs, uploads or stores a secret value, and no credential value
  appears in a report, an error or a tooltip.
- **It does not refresh tokens.** For providers whose session belongs to another
  tool (Codex, Claude, Cursor, Gemini), token refresh is delegated to the CLI
  that owns the file (`codex login`, `claude`, …). When such a session is dead,
  the app shows an actionable "re-authenticate" state instead of trying to fix
  it. This matches how the original app behaves.
- The **one** exception where this app owns a credential is Copilot: its device-
  flow token is written atomically to this app's own config
  (`%APPDATA%\CodexBar\config.json`) and is never printed.
- E-mail addresses and account IDs shown in the UI are **masked at the provider
  boundary** (`user@…`, `abcd1234…`) before they ever leave a fetcher.

## Scope and limitations

- **14 of ~85 providers.** The port covers the providers listed above; the rest
  are unimplemented.
- **No cost or usage-history views**, **no notifications**, **no desktop
  widget**.
- **No packaged installer.** You build and run the binaries yourself.
- New endpoints are pinned to HTTPS and validated before use; where an endpoint
  or a credential shape could not be verified against a live account, the
  behaviour is documented rather than guessed (see `docs/port-specs/`).

## Repository layout

```
winapp/
├─ crates/
│  ├─ codexbar-core/       shared types, the Provider trait, mock data
│  ├─ codexbar-providers/   the 14 real fetchers + HTTP/credential/OAuth helpers
│  └─ codexbar-cli/         the `codexbar` binary
├─ src-tauri/               the tray app (Tauri 2): tray, popover, settings, login
├─ ui/                      the static frontend (embedded at compile time)
├─ scripts/                 local build/verification helpers
└─ docs/port-specs/         the Windows port specifications
```

`CONTRACT.md` documents the frozen data contract every provider must honour.

## License & attribution

This project is **MIT-licensed** — see [`LICENSE`](LICENSE).

It is a port of **[CodexBar](https://github.com/steipete/CodexBar)** by
**Peter Steinberger**, also MIT. The original's design, data model and product
invariants (read-only credential access, never refreshing shared tokens,
delegating recovery to the owning CLI) are carried over deliberately; all
Windows-specific code here is original.

- Original: <https://github.com/steipete/CodexBar>
- This port: <https://github.com/EndingVE/Codex-Windows-Port>
