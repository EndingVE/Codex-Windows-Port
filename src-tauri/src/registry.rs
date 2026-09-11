//! Provider registry for the tray app.
//!
//! **One decision, in one place: live or mock.**
//!
//! * **live** (the default) — `codexbar_providers::live_registry()`, i.e. the
//!   *exact same* registry `codexbar usage --live` builds. Real credentials are
//!   read from this machine (`OPENROUTER_API_KEY`, `%APPDATA%\CodexBar\config.json`,
//!   `~/.codex/auth.json`, the Claude credential store, …) and each provider
//!   fetches its own usage. A provider with no credentials returns
//!   `FetchStatus::NotConfigured` **without** opening a socket, a provider whose
//!   fetcher has not landed yet returns `FetchStatus::Error` naming the file to
//!   edit, and nothing is ever invented.
//! * **mock** — only when the user explicitly asks for it
//!   (`mockMode: true` in `config.json`, or the settings page). Deterministic
//!   sample data, `DataSource::Mock` on every card, no credential read, no
//!   network call. It exists so a screenshot or a bug report can be reproduced
//!   on any machine.
//!
//! There is deliberately **no automatic fallback to mock**: silently swapping in
//! fake numbers when credentials are missing is the one thing that would make
//! the tray lie. Missing credentials show up as `notConfigured` cards instead.
//!
//! `collect()` sorts by `ProviderId::ALL`, so both modes emit the same 14
//! providers in the same order and the popover cannot tell them apart.

use codexbar_core::{collect, mock::MockProvider, Provider, ProviderId, UsageReport};

use crate::settings::Settings;

/// Which provider set the app is serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryMode {
    /// Real fetchers, real credentials, `codexbar-providers`.
    Live,
    /// Deterministic sample data, offline.
    Mock,
}

impl RegistryMode {
    /// Wire value, also used by `app_metadata.mode` and the evidence scripts.
    pub const fn as_str(self) -> &'static str {
        match self {
            RegistryMode::Live => "live",
            RegistryMode::Mock => "mock",
        }
    }

    /// Live unless the user explicitly asked for sample data.
    pub fn from_settings(settings: &Settings) -> Self {
        if settings.mock_mode {
            RegistryMode::Mock
        } else {
            RegistryMode::Live
        }
    }
}

impl std::fmt::Display for RegistryMode {
    /// So `eprintln!("… {mode} mode …")` and friends print `live` / `mock`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Sample-data registry: every provider is a [`MockProvider`].
pub fn mock_registry() -> Vec<Box<dyn Provider>> {
    ProviderId::ALL
        .into_iter()
        .map(|id| Box::new(MockProvider::new(id)) as Box<dyn Provider>)
        .collect()
}

/// Live registry.
///
/// Deliberately a one-line forward to `codexbar_providers::live_registry()` so
/// the tray app and `codexbar usage --live` can never disagree about which
/// providers exist or where their numbers come from. Provider workers register
/// their fetcher there and nothing here changes.
pub fn live_registry() -> Vec<Box<dyn Provider>> {
    codexbar_providers::live_registry()
}

/// The provider list for `mode`.
pub fn registry_for(mode: RegistryMode) -> Vec<Box<dyn Provider>> {
    match mode {
        RegistryMode::Live => live_registry(),
        RegistryMode::Mock => mock_registry(),
    }
}

/// Fetch every provider of `mode` into one report (canonical order).
pub fn report_for(mode: RegistryMode) -> UsageReport {
    collect(&registry_for(mode))
}

/// Fetch the report the current settings call for.
pub fn report_for_settings(settings: &Settings) -> UsageReport {
    report_for(RegistryMode::from_settings(settings))
}

// ---------------------------------------------------------------------------
// `refreshCredentials` extension point
// ---------------------------------------------------------------------------

/// What the optional proactive credential-refresh pass did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CredentialRefreshOutcome {
    /// True when `codexbar-providers` exposes a hook this pass can call.
    pub hook_available: bool,
    /// Providers whose stored credentials were actually refreshed.
    pub refreshed: Vec<ProviderId>,
}

/// Entry point this pass will call once the provider crate grows one.
///
/// TODO(providers): `codexbar-providers` only ever writes credentials through
/// `oauth::refresh` (see `crates/codexbar-providers/src/oauth.rs`), and today it
/// is the *provider* that decides to call it. When the crate exposes a
/// proactive, whole-registry entry point — e.g.
/// `codexbar_providers::refresh_expired_credentials(&registry) -> Vec<(ProviderId, Result<(), String>)>`
/// — call it from here, map the results onto [`CredentialRefreshOutcome`], and
/// flip [`CredentialRefreshOutcome::hook_available`] to `true`.
pub const CREDENTIAL_REFRESH_ENTRY_POINT: &str =
    "crates/codexbar-providers/src/oauth.rs (proactive, whole-registry refresh)";

/// Run the proactive credential-refresh pass — **only** ever called when
/// `refreshCredentials` is on.
///
/// With the setting off the app performs no credential write of any kind
/// (frozen contract: credentials are read-only except for a user-initiated
/// refresh). With the setting on this build still reports
/// `hook_available: false` and refreshes nothing, because no provider exposes
/// the hook yet — it does not pretend to have refreshed anything.
pub fn refresh_credentials_pass(registry: &[Box<dyn Provider>]) -> CredentialRefreshOutcome {
    if registry.is_empty() {
        return CredentialRefreshOutcome::default();
    }
    CredentialRefreshOutcome {
        hook_available: false,
        refreshed: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexbar_core::DataSource;

    /// The tray must always render a card for all 14 providers, whatever the
    /// status — a missing provider silently disappears from the menu otherwise.
    #[test]
    fn registry_covers_every_provider_once_in_canonical_order() {
        let report = report_for(RegistryMode::Mock);
        let ids: Vec<ProviderId> = report.providers.iter().map(|p| p.provider).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
        assert_eq!(ids.len(), 14);
    }

    /// Live is the default mode; the mock registry is opt-in only.
    #[test]
    fn mode_follows_the_mock_mode_setting() {
        assert_eq!(
            RegistryMode::from_settings(&Settings::default()),
            RegistryMode::Live
        );
        assert_eq!(
            RegistryMode::from_settings(&Settings::default()).as_str(),
            "live"
        );

        let asked = Settings {
            mock_mode: true,
            ..Settings::default()
        };
        assert_eq!(
            RegistryMode::from_settings(&asked),
            RegistryMode::Mock,
            "mock must require an explicit mockMode: true"
        );
        assert_eq!(RegistryMode::from_settings(&asked).as_str(), "mock");
    }

    /// Mock mode is the offline path: every card says `mock`, which is the same
    /// signal `ui/app.js` turns into the "sample data" banner. A mock provider
    /// touches neither the environment nor a socket. (Its *status* is still
    /// whatever the sample data says — mock covers healthy / error /
    /// not-configured too; only the source is uniform.)
    #[test]
    fn mock_mode_is_labelled_mock_on_every_card() {
        let report = report_for(RegistryMode::Mock);
        for snapshot in &report.providers {
            assert_eq!(snapshot.source, DataSource::Mock, "{}", snapshot.provider);
        }
    }

    /// The app's live registry is the CLI's live registry: same 14 ids in the
    /// same order, no fetch (constructing a provider must stay offline).
    #[test]
    fn the_app_live_registry_is_the_cli_live_registry() {
        let app: Vec<ProviderId> = live_registry().iter().map(|p| p.id()).collect();
        let cli: Vec<ProviderId> = codexbar_providers::live_registry()
            .iter()
            .map(|p| p.id())
            .collect();
        assert_eq!(app, cli);
        assert_eq!(app, ProviderId::ALL.to_vec());
    }

    /// With `refreshCredentials` on, the pass is honest about having no hook yet
    /// rather than claiming a refresh that never happened.
    #[test]
    fn credential_refresh_pass_reports_no_hook_without_inventing_results() {
        let outcome = refresh_credentials_pass(&mock_registry());
        assert!(!outcome.hook_available);
        assert!(outcome.refreshed.is_empty());
        assert!(CREDENTIAL_REFRESH_ENTRY_POINT.contains("oauth.rs"));
    }
}
