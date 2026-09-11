# The frozen data contract (`codexbar-core`)

**Read this before writing a provider.** It exists so a provider worker can land
their code without touching shared types, and so the tray app, the CLI and the UI
can never disagree about a payload.

## Where it lives

```
crates/codexbar-core/src/types.rs         ← the contract (types + Provider trait)
crates/codexbar-core/src/mock.rs          ← sample data used by every consumer
crates/codexbar-providers/src/            ← real fetchers + shared helpers
crates/codexbar-providers/src/providers/openrouter.rs  ← the reference provider
```

`codexbar-core` is frozen for provider workers: they add a module under
`codexbar-providers/src/providers/` and touch nothing else. See
[Adding a provider](#adding-a-provider--the-whole-job) below.

Reference for the original: `repo/Sources/CodexBarCore/UsageFetcher.swift`
(`struct RateWindow`, lines 1–96) is the macOS equivalent of `RateWindow` +
`NamedRateWindow` below.

## Version

`SCHEMA_VERSION = 1` — emitted as `schemaVersion` in every JSON payload.
Add fields as `Option<T>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`
and bump the constant only when you'd break an existing reader.

## Wire format rules

1. **Keys are `camelCase`** (`usedPercent`, `resetsAt`, `windowMinutes`,
   `isSyntheticPlaceholder`). This matches the Swift port so both can share fixtures.
2. **Timestamps are RFC 3339 UTC strings at millisecond precision**, e.g.
   `"2026-09-10T20:58:33.577Z"`. Implemented by `types::rfc3339` / `types::rfc3339::option`.
3. **Absent optionals are omitted, never `null`.** A consumer can test
   `if 'x' in obj` on either side.
4. **Booleans that default to `true` are omitted when `true`** (`usageKnown`).
5. **`isSyntheticPlaceholder` is omitted when `false`** — its presence is the signal.

## Types at a glance

| Rust | JSON key(s) | Meaning |
| --- | --- | --- |
| `ProviderId` | `provider` | 14 ids, in canonical order: `codex`, `claude`, `cursor`, `openrouter`, `copilot`, `gemini`, `deepseek`, `groq`, `zai`, `minimax`, `kimi`, `elevenlabs`, `xai`, `opencodego` |
| `AuthKind` | — | `apiKey`, `localOAuthFile`, `deviceFlow` — declared per provider, so the UI can pick the right setup hint without asking the provider |
| `WindowKind` | `kind` | `session`, `weekly`, `weeklyScoped`, `extra` |
| `RateWindow` | `window` | `usedPercent`, `windowMinutes?`, `resetsAt?`, `resetDescription?`, `nextRegenPercent?`, `isSyntheticPlaceholder?` |
| `NamedRateWindow` | — | `id`, `title`, `kind`, `window`, `usageKnown?` |
| `MoneyBalance` | `balance` | `amount`, `currency`, `label?` |
| `FetchStatus` | `status` | `ok`, `stale`, `error`, `notConfigured` |
| `DataSource` | `source` | `apiKey`, `oauth`, `cli`, `web`, `mock` |
| `ProviderSnapshot` | — | one provider card: identity, `windows[]`, `balance?`, `status`, `error?`, `source`, `fetchedAt` |
| `UsageReport` | — | `schemaVersion`, `generatedAt`, `providers[]` (canonical order) |

### Rules the UI depends on

* `usedPercent` is **not clamped** — providers may exceed 100. Display code clamps.
  `RateWindow::display_clamped()` and `remaining_percent()` are the display projections.
* A window with `isSyntheticPlaceholder: true` means "this lane does not exist",
  **not** "0% used". Keep it out of headline/menu decisions (the macOS
  `5h 0%`-from-`null` bug). `ProviderSnapshot::headline_window()` already filters it.
* `usageKnown: false` means reset metadata exists but the usage number is not real.
  Render the label and reset, not a percentage.
* Never mix identity fields across providers: `account`/`plan` must come from the
  same provider as the `windows` in that `ProviderSnapshot`.
* `status` drives the card: `notConfigured` → setup hint, `error` → error box,
  `stale` → keep last good numbers and say how old they are.

## Adding a provider — the whole job

A real provider lives in **`crates/codexbar-providers/src/providers/<name>.rs`**,
never in `codexbar-core`. `crates/codexbar-providers/src/providers/openrouter.rs`
is the reference implementation; copy its shape:

```rust
// crates/codexbar-providers/src/providers/claude.rs
use std::sync::Arc;
use codexbar_core::{DataSource, FetchStatus, NamedRateWindow, ProviderId, ProviderSnapshot,
                    RateWindow, WindowKind};
use codexbar_providers::{Env, HttpClient};
use chrono::{DateTime, Utc};

pub struct ClaudeOAuth { client: Arc<dyn HttpClient>, env: Env }

impl ClaudeOAuth {
    /// Production: real HTTPS client, process environment, port config.
    pub fn new() -> Self { Self { client: codexbar_providers::shared_client(), env: Env::from_process() } }
    /// Tests: fixture client, injected environment, no disk, no network.
    pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self { Self { client, env } }
}

impl codexbar_core::Provider for ClaudeOAuth {
    fn id(&self) -> ProviderId { ProviderId::Claude }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        // 1. credentials — Secret never prints itself; missing => NotConfigured
        // 2. endpoint policy — secure_base_url() fails closed on http://
        // 3. call the API through self.client (never reqwest directly)
        // 4. map onto RateWindow; on failure return an Error snapshot
        todo!("your provider")
    }
}
```

Then:

1. `crates/codexbar-providers/src/providers/mod.rs` — add `pub mod claude;`
2. `crates/codexbar-providers/src/lib.rs` — in `live_registry()`, wire that
   provider to `ClaudeOAuth::with_client(...)` (every shipping provider is
   already wired; the `PendingProvider` fallback is unreachable).
3. `crates/codexbar-providers/tests/claude_fixture.rs` — fixture tests, offline.

That's the entire integration surface. **The tray app already picks it up**: the
CLI's registry (`crates/codexbar-cli/src/registry.rs`) calls
`codexbar_providers::live_registry()` for `--live`, and the GUI's registry
(`src-tauri/src/registry.rs`) iterates `ProviderId::ALL`. `collect()` sorts by
`ProviderId::ALL`, the tray repaints from `max_used_percent()`, and the UI renders
whatever `windows` you return.

All 14 shipping providers are implemented. For a hypothetical new provider,
`PendingProvider` still exists and returns `FetchStatus::Error` with a "pending
provider worker" message — deliberately **not** `notConfigured`, because telling
a user to configure credentials for something nobody implemented is a lie.

### Helpers you must reuse (don't re-implement these)

| Need | Use |
| --- | --- |
| Attach a bearer without it ever printing | `Secret` + `HttpRequest::bearer` |
| Read an env var (Windows names are case-insensitive) | `Env::get` / `Env::first_of` |
| Apply the canonical trim/unquote rule (`SPEC-apikey.md` §1.2) | `cleaned()` |
| The port's own `config.json` (`%APPDATA%\CodexBar\config.json`, `CODEXBAR_CONFIG`) | `PortConfig::load` + `resolve_api_key` |
| Validate a user-supplied base URL | `secure_base_url` (HTTPS-only, no userinfo, no encoded delimiters) |
| Show a key in `account`/`error` | `Secret::redacted` → `sk-or-1v…9f2c` |
| Test without network | `testing::FixtureClient` + `FixtureResponse` |
| An explicit OAuth refresh | `oauth::refresh` (+ `write_json_atomic` for the result) |

### Non-negotiables

* **Never panic.** Catch errors and return `FetchStatus::Error` with a message.
  A panicking provider takes down the tray app. (This applies to test doubles too.)
* **No credential values in `error`, `account`, or `title`.** Mask tokens
  (`sk-or-v1…9f2c`, which is what the mock does). Hard-code no secrets in fixtures.
* **`Provider::fetch` takes `now`** — use it instead of calling `Utc::now()`
  inside, so tests and fixtures stay deterministic.
* **Return `notConfigured`, not `error`,** when the user simply has no credentials:
  the UI turns that into an actionable setup hint instead of a scary failure.
* **Tests never touch the network.** Inject a `FixtureClient`.
* **Only ever write credentials through `oauth::write_json_atomic`** (user-initiated
  refresh, atomic + `.bak`). Every other code path is read-only.
* Respect the refresh cadence: `fetch` is called from a background thread every
  `REFRESH_INTERVAL` (60 s today). Keep it bounded and non-interactive — that is
  why OpenRouter's optional `/key` probe carries a 1 s deadline and degrades softly.

## Data sources and `--live`

`codexbar usage` reads the **mock** registry by default: deterministic sample data,
no credentials, no network — every file in `evidence/` comes from it.
`codexbar usage --live` reads `codexbar_providers::live_registry()`: real
credentials, real endpoints, same JSON contract. Both modes emit the same 14
providers in the same order, so the UI cannot tell them apart.
