//! `codexbar-providers` — real provider implementations for the CodexBar Windows
//! port, plus the helpers every provider worker shares.
//!
//! `codexbar-core` is the frozen contract; this crate is where the contract gets
//! filled in. It is deliberately **not** a dependency of `src-tauri`, so the GUI
//! build never grows an HTTP/TLS stack it does not need yet.
//!
//! # Shape of the crate
//!
//! | Module | What it gives a provider worker |
//! | --- | --- |
//! | [`http`] | `HttpRequest`/`HttpResponse`, the [`HttpClient`] seam, the HTTPS endpoint policy, a redacting `Debug` |
//! | [`credential`] | `Secret` (prints as `<redacted>`), the canonical `cleaned()` rule, a case-insensitive `Env`, the read-only `PortConfig` |
//! | [`oauth`] | refresh-token grant + the crate's only credential write (atomic, with `.bak`) |
//! | [`testing`] | `FixtureClient` so provider tests never touch the network |
//! | [`providers`] | one module per provider; [`OpenRouter`] is the reference |
//!
//! # Adding a provider (the whole job)
//!
//! ```ignore
//! // 1. src/providers/groq.rs
//! pub struct Groq { client: Arc<dyn HttpClient>, env: Env }
//! impl Groq {
//!     pub fn new() -> Self { Self { client: shared_client(), env: Env::from_process() } }
//!     pub fn with_client(client: Arc<dyn HttpClient>, env: Env) -> Self { … }
//! }
//! impl Provider for Groq {
//!     fn id(&self) -> ProviderId { ProviderId::Groq }
//!     fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot { /* never panics */ }
//! }
//!
//! // 2. src/providers/mod.rs         → pub mod groq;
//! // 3. src/lib.rs (live_registry)   → Groq::new() instead of PendingProvider
//! // 4. tests/groq_fixture.rs        → fixtures, no network
//! ```
//!
//! # Non-negotiables (same as `CONTRACT.md`)
//!
//! * `fetch` never panics and never blocks on user input; failures become
//!   `FetchStatus::Error` with a message.
//! * Missing credentials are `FetchStatus::NotConfigured`, not `Error`.
//! * No credential value ever appears in `error`, `account`, `plan` or a `title`.
//!   Use [`Secret::redacted`].
//! * `fetch` uses the `now` it is given, never `Utc::now()`.
//! * Tests are offline: [`testing::FixtureClient`], no sockets.

pub mod credential;
pub mod http;
pub mod oauth;
pub mod providers;
pub mod testing;

pub use credential::{
    api_key_from_env, cleaned, first_existing, home_dir, read_json, read_secret_file, redact,
    redact_secrets_in_text, CredentialError, Env, PortConfig, Secret,
};
pub use http::{
    default_client, ensure_https, host_is_allowed, secure_base_url, shared_client, FailingClient,
    HttpClient, HttpError, HttpErrorKind, HttpRequest, HttpResponse, Method,
};
pub use oauth::{
    refresh, write_json_atomic, OAuthError, RefreshRequest, RefreshedToken, REFRESH_TIMEOUT,
};
pub use providers::{
    ClaudeOAuth, Codex, Copilot, Cursor, DeepSeek, Gemini, Groq, OpenCodeGo, OpenCodePaths,
    OpenRouter, PendingProvider,
};
pub use providers::{ElevenLabs, Kimi, MiniMax, Xai, Zai};
pub use testing::{CapturedRequest, FixtureClient, FixtureResponse};

use std::sync::Arc;

use codexbar_core::{Provider, ProviderId};

/// Every provider this build can fetch, in `ProviderId::ALL` order.
///
/// All 14 providers have a real fetcher wired here; the [`PendingProvider`]
/// fallback below is unreachable in this build and is kept only so a future
/// provider id cannot silently fall off the list. This is the **one thing** a
/// provider worker edits.
pub fn live_registry() -> Vec<Box<dyn Provider>> {
    live_registry_with(shared_client())
}

/// Same as [`live_registry`] but with an injected client.
///
/// Used by tests (a [`FixtureClient`] keeps the whole report offline) and by any
/// future embedder that wants to share one connection pool.
pub fn live_registry_with(client: Arc<dyn HttpClient>) -> Vec<Box<dyn Provider>> {
    use providers::pending::PendingProvider;

    let env = Env::from_process();
    let config = PortConfig::load(&env).unwrap_or(None);

    ProviderId::ALL
        .into_iter()
        .map(|id| -> Box<dyn Provider> {
            match id {
                // ---- implemented ------------------------------------------
                ProviderId::Codex => Box::new(Codex::with_client(Arc::clone(&client), env.clone())),
                ProviderId::OpenRouter => Box::new(
                    OpenRouter::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                ProviderId::DeepSeek => Box::new(
                    DeepSeek::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                ProviderId::Groq => Box::new(
                    Groq::with_client(Arc::clone(&client), env.clone()).with_config(config.clone()),
                ),
                // OpenCode Go also reads its device-local store, so the paths are
                // resolved here (production) rather than inside `fetch`.
                ProviderId::OpenCodeGo => Box::new(
                    OpenCodeGo::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone())
                        .with_paths(OpenCodePaths::from_env(&env)),
                ),
                ProviderId::Copilot => Box::new(
                    Copilot::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                // Claude reads Claude Code's own credential file (read-only) and
                // calls the OAuth usage/profile API.
                ProviderId::Claude => {
                    Box::new(ClaudeOAuth::with_client(Arc::clone(&client), env.clone()))
                }
                // Cursor reads its own VS Code global state (read-only) and calls
                // cursor.com's usage-summary / auth endpoints.
                ProviderId::Cursor => Box::new(
                    Cursor::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                // Gemini reads the Gemini CLI OAuth session (read-only) and calls
                // Google's Cloud Code quota/tier APIs.
                ProviderId::Gemini => Box::new(
                    Gemini::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                ProviderId::Kimi => Box::new(
                    Kimi::with_client(Arc::clone(&client), env.clone()).with_config(config.clone()),
                ),
                ProviderId::ElevenLabs => Box::new(
                    ElevenLabs::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                ProviderId::Xai => Box::new(
                    Xai::with_client(Arc::clone(&client), env.clone()).with_config(config.clone()),
                ),
                // z.ai / GLM: quota (+ CN-only best-effort balance). The token
                // chain is region-aware, so the region is resolved inside `fetch`.
                ProviderId::Zai => Box::new(
                    Zai::with_client(Arc::clone(&client), env.clone()).with_config(config.clone()),
                ),
                ProviderId::MiniMax => Box::new(
                    MiniMax::with_client(Arc::clone(&client), env.clone())
                        .with_config(config.clone()),
                ),
                // Every `ProviderId` has a real fetcher in this build; the arm is
                // kept so a future provider id cannot silently fall off the list.
                #[allow(unreachable_patterns)]
                other => Box::new(PendingProvider::new(other)),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use codexbar_core::{collect, FetchStatus};

    use testing::FixtureResponse;

    /// 14 providers, canonical order, one entry each.
    #[test]
    fn live_registry_covers_every_provider_once_in_canonical_order() {
        let client: Arc<dyn HttpClient> = Arc::new(FixtureClient::empty());
        let registry = live_registry_with(client);
        let ids: Vec<ProviderId> = registry.iter().map(|p| p.id()).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
        assert_eq!(ids.len(), 14);
    }

    /// The whole live report must build offline. Providers with no credential in
    /// the fixture environment must report `notConfigured` **without** sending a
    /// request. Every provider in this build has a real fetcher.
    #[test]
    fn live_report_offline_is_honest_and_never_panics() {
        let fixture = Arc::new(FixtureClient::empty());
        let client: Arc<dyn HttpClient> = Arc::clone(&fixture) as Arc<dyn HttpClient>;
        let report = collect(&live_registry_with(client));

        assert_eq!(report.providers.len(), 14);
        let minimax_configured =
            report.get(ProviderId::MiniMax).unwrap().status != FetchStatus::NotConfigured;
        if !minimax_configured {
            assert_eq!(
                fixture.request_count(),
                0,
                "no credentials means no network, not even a probe"
            );
        }

        // Providers with a real fetcher wired up in this build.
        let implemented = [
            ProviderId::Claude,
            ProviderId::Codex,
            ProviderId::OpenRouter,
            ProviderId::DeepSeek,
            ProviderId::Groq,
            ProviderId::OpenCodeGo,
            ProviderId::Copilot,
            ProviderId::Kimi,
            ProviderId::ElevenLabs,
            ProviderId::Xai,
            ProviderId::Zai,
            ProviderId::MiniMax,
            ProviderId::Cursor,
            ProviderId::Gemini,
        ];
        let openrouter = report.get(ProviderId::OpenRouter).unwrap();
        assert_eq!(openrouter.status, FetchStatus::NotConfigured);
        assert!(openrouter.account.is_none());
        assert!(openrouter
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("OPENROUTER_API_KEY"));

        for id in [ProviderId::DeepSeek, ProviderId::Groq] {
            let snapshot = report.get(id).unwrap();
            assert_eq!(snapshot.status, FetchStatus::NotConfigured);
            assert!(snapshot.account.is_none());
            assert!(snapshot.windows.is_empty());
        }
        assert!(report
            .get(ProviderId::DeepSeek)
            .unwrap()
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("DEEPSEEK_API_KEY"));
        assert!(report
            .get(ProviderId::Groq)
            .unwrap()
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("GROQ_SESSION_TOKEN"));

        // OpenCode Go reads a device-local SQLite store, so its offline status
        // depends on the machine: an estimated `ok` snapshot where OpenCode has
        // run, `notConfigured` where it has not. Either way no request is sent.
        let opencodego = report.get(ProviderId::OpenCodeGo).unwrap();
        assert!(
            matches!(
                opencodego.status,
                FetchStatus::NotConfigured | FetchStatus::Ok
            ),
            "unexpected OpenCode Go status: {:?}",
            opencodego.status
        );
        if opencodego.status == FetchStatus::Ok {
            assert!(opencodego.account.is_none());
            assert_eq!(opencodego.source, codexbar_core::DataSource::Cli);
            assert!(!opencodego.windows.is_empty());
        }

        // Copilot has no third-party file to scrape: with no stored token it is
        // `notConfigured`, points at the device-flow setup, and sends nothing.
        let copilot = report.get(ProviderId::Copilot).unwrap();
        assert_eq!(copilot.status, FetchStatus::NotConfigured);
        assert!(copilot.account.is_none());
        assert!(copilot.windows.is_empty());
        assert!(copilot
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("COPILOT_API_TOKEN"));

        // Claude reads Claude Code's credential file read-only. With no usable
        // credential it is `notConfigured`; on this machine the file exists but the
        // token is expired, which is an honest `error` naming the re-auth action.
        // Either way nothing is written and no request is sent.
        let claude = report.get(ProviderId::Claude).unwrap();
        assert!(
            matches!(
                claude.status,
                FetchStatus::NotConfigured | FetchStatus::Error
            ),
            "unexpected Claude status: {:?}",
            claude.status
        );
        assert!(claude.windows.is_empty());

        // Kimi, ElevenLabs and xAI read only env/config credentials: with none
        // present they are `notConfigured`, name their env var, and send nothing.
        for id in [ProviderId::Kimi, ProviderId::ElevenLabs, ProviderId::Xai] {
            let snapshot = report.get(id).unwrap();
            assert_eq!(snapshot.status, FetchStatus::NotConfigured);
            assert!(snapshot.account.is_none());
            assert!(snapshot.windows.is_empty());
            assert!(snapshot.balance.is_none());
        }
        for (id, variable) in [
            (ProviderId::Kimi, "KIMI_CODE_API_KEY"),
            (ProviderId::ElevenLabs, "ELEVENLABS_API_KEY"),
            (ProviderId::Xai, "XAI_MANAGEMENT_API_KEY"),
        ] {
            assert!(report
                .get(id)
                .unwrap()
                .error
                .as_deref()
                .unwrap_or_default()
                .contains(variable));
        }

        // z.ai has no credential in the fixture environment: `notConfigured`,
        // names its env var, sends nothing. Its region-aware alias chain,
        // quota mapping and balance lane live in `tests/zai_fixture.rs`.
        let zai = report.get(ProviderId::Zai).unwrap();
        assert_eq!(zai.status, FetchStatus::NotConfigured);
        assert!(zai.account.is_none());
        assert!(zai.windows.is_empty());
        assert!(zai
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("Z_AI_API_KEY"));

        // MiniMax is environment-dependent: `MINIMAX_API_KEY` is set on the
        // machine running these tests, so it may legitimately probe — against the
        // fixture client, never the network. Its value is never read here.
        let minimax = report.get(ProviderId::MiniMax).unwrap();
        if minimax_configured {
            let account = minimax.account.as_deref().unwrap_or_default();
            assert!(
                account.contains('…'),
                "the MiniMax token must be masked, got {account:?}"
            );
            // The probe goes to MiniMax's own API host (the fixture client is the
            // only transport, so nothing leaves the machine).
            let captured = fixture.captured();
            let contacted: Vec<&str> = captured
                .iter()
                .map(|request| request.url.as_str())
                .collect();
            assert!(
                contacted
                    .iter()
                    .any(|url| url.contains("api.minimax.io") || url.contains("api.minimaxi.com")),
                "MiniMax should probe its own API host, got {contacted:?}"
            );
        } else {
            assert!(minimax.account.is_none());
            assert!(minimax.windows.is_empty());
            assert!(minimax
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("MINIMAX_API_KEY"));
        }

        // Cursor and Gemini read another tool's local session (Cursor's VS Code
        // global-state DB / the Gemini CLI's `~/.gemini`). Neither exists in the
        // fixture environment, so both are `notConfigured` and send nothing.
        for id in [ProviderId::Cursor, ProviderId::Gemini] {
            let snapshot = report.get(id).unwrap();
            assert_eq!(snapshot.status, FetchStatus::NotConfigured, "{id:?}");
            assert!(snapshot.account.is_none());
            assert!(snapshot.windows.is_empty());
        }

        // Defensive: every `ProviderId` in this build has a real fetcher, so this
        // loop is a no-op today. It stays so a newly added id that is not yet
        // wired is caught instead of silently reported as configured.
        for snapshot in &report.providers {
            if implemented.contains(&snapshot.provider) {
                continue;
            }
            assert_eq!(snapshot.status, FetchStatus::Error);
            assert!(snapshot
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("pending provider worker"));
        }
    }

    /// A credential must never appear in the payload, no matter how the provider
    /// fails — the report is what gets screenshotted and pasted into issues.
    #[test]
    fn no_payload_field_ever_carries_the_api_key() {
        let secret = "sk-or-v1-0123456789abcdefghijklmnop";
        let client = Arc::new(FixtureClient::new(vec![FixtureResponse::json(
            401,
            r#"{"error":{"message":"No auth credentials found"}}"#,
        )]));
        let env = Env::empty().with("OPENROUTER_API_KEY", secret);
        let provider = OpenRouter::with_client(Arc::clone(&client) as Arc<dyn HttpClient>, env);
        let registry: Vec<Box<dyn Provider>> = vec![Box::new(provider)];
        let report = collect(&registry);

        let rendered = serde_json::to_string(&report).unwrap();
        assert!(!rendered.contains(secret));
        let snapshot = report.get(ProviderId::OpenRouter).unwrap();
        assert_eq!(snapshot.status, FetchStatus::Error);
        assert!(snapshot
            .account
            .as_deref()
            .unwrap_or_default()
            .contains('…'));
        let _ = Utc::now();
    }
}
