//! Providers that do not have a fetcher yet.
//!
//! `live_registry()` returns a real [`crate::OpenRouter`] plus one
//! [`PendingProvider`] for each of the other 13 providers. That keeps
//! `codexbar usage --live` honest and end-to-end testable from day one:
//!
//! * a provider with a pending fetcher reports `FetchStatus::Error` with a
//!   message that names the file to edit — **not** `notConfigured`, because
//!   telling a user to configure credentials for something nobody implemented
//!   would be a lie;
//! * swapping in a real implementation is a two-line change in
//!   [`crate::live_registry`] and nothing else moves.

use chrono::{DateTime, Utc};
use codexbar_core::{AuthKind, DataSource, FetchStatus, Provider, ProviderId, ProviderSnapshot};

/// Where a provider worker adds the real fetcher.
pub const WORKER_ENTRY_POINT: &str = "crates/codexbar-providers/src/providers";

/// A provider whose fetcher has not been written yet.
#[derive(Debug, Clone, Copy)]
pub struct PendingProvider {
    id: ProviderId,
}

impl PendingProvider {
    pub const fn new(id: ProviderId) -> Self {
        Self { id }
    }

    /// Which data source this provider *will* use, so the UI badge is at least
    /// the right shape while the fetcher is missing.
    pub const fn planned_source(self) -> DataSource {
        match self.id.auth_kind() {
            AuthKind::ApiKey | AuthKind::DeviceFlow => DataSource::ApiKey,
            AuthKind::LocalOAuthFile => DataSource::OAuth,
        }
    }
}

impl Provider for PendingProvider {
    fn id(&self) -> ProviderId {
        self.id
    }

    fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: self.id,
            title: self.id.title().to_string(),
            account: None,
            plan: None,
            windows: Vec::new(),
            balance: None,
            status: FetchStatus::Error,
            error: Some(format!(
                "{} has no fetcher wired up in this build yet (pending provider worker). \
                 Add a providers/{}.rs module next to providers/openrouter.rs under \
                 {WORKER_ENTRY_POINT} and swap this stub in codexbar_providers::live_registry().",
                self.id.title(),
                self.id.as_str()
            )),
            source: self.planned_source(),
            fetched_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_providers_say_so_explicitly() {
        let now = Utc::now();
        let provider = PendingProvider::new(ProviderId::Groq);
        let snapshot = provider.fetch(now);
        assert_eq!(snapshot.provider, ProviderId::Groq);
        assert_eq!(snapshot.status, FetchStatus::Error);
        assert!(snapshot.windows.is_empty());
        assert!(snapshot
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("pending provider worker"));
        // Not `notConfigured`: there is nothing for the user to configure yet.
        assert_ne!(snapshot.status, FetchStatus::NotConfigured);
    }

    #[test]
    fn planned_source_follows_the_auth_kind() {
        assert_eq!(
            PendingProvider::new(ProviderId::Claude).planned_source(),
            DataSource::OAuth
        );
        assert_eq!(
            PendingProvider::new(ProviderId::MiniMax).planned_source(),
            DataSource::ApiKey
        );
    }
}
