//! Provider registry.
//!
//! **This is the one place provider workers touch** on the CLI side. The mock
//! registry is what every fixture and screenshot uses; the live registry hands
//! off to `codexbar-providers`, which owns the real fetchers.
//!
//! Keep both lists ordered like `ProviderId::ALL`; `collect()` re-sorts anyway.

use codexbar_core::{collect, mock::MockProvider, Provider, ProviderId, UsageReport};

/// Mock providers — deterministic sample data, no credentials, no network.
///
/// Used by `codexbar usage` (the default) and therefore by every file in
/// `evidence/`, so a screenshot can never depend on a live account.
pub fn registry() -> Vec<Box<dyn Provider>> {
    ProviderId::ALL
        .into_iter()
        .map(|id| Box::new(MockProvider::new(id)) as Box<dyn Provider>)
        .collect()
}

/// Fetch every mock provider into one report.
pub fn current_report() -> UsageReport {
    collect(&registry())
}

/// Real providers — `codexbar usage --live`.
///
/// The provider list itself lives in `codexbar_providers::live_registry()`; this
/// wrapper exists so the CLI has exactly one import site for "which providers
/// does this build have", mirroring the tray app's `src-tauri/src/registry.rs`.
pub fn live_report() -> UsageReport {
    collect(&codexbar_providers::live_registry())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_provider_once() {
        let report = current_report();
        let ids: Vec<ProviderId> = report.providers.iter().map(|p| p.provider).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
        assert_eq!(report.schema_version, codexbar_core::SCHEMA_VERSION);
        assert_eq!(ids.len(), 14);
    }

    /// The live registry must expose the same 14 providers in the same order, so
    /// `--live` and mock payloads are shaped identically (the UI cannot tell them
    /// apart, and `collect()` keeps the canonical order in both modes).
    #[test]
    fn live_registry_matches_the_mock_shape() {
        let live: Vec<ProviderId> = codexbar_providers::live_registry()
            .iter()
            .map(|p| p.id())
            .collect();
        assert_eq!(live, ProviderId::ALL.to_vec());
    }
}
