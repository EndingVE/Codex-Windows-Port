//! `codexbar-core` — the frozen shared contract for the CodexBar Windows port.
//!
//! Read [`types`] first. Provider workers add a module implementing
//! [`types::Provider`] and register it in the app's registry; nothing in this
//! crate should need editing.
//!
//! ```no_run
//! use codexbar_core::{mock, types::{collect, ProviderId}};
//!
//! let report = collect(&mock::mock_registry());
//! assert_eq!(report.providers.len(), ProviderId::ALL.len());
//! println!("{}", report);
//! ```

pub mod mock;
pub mod types;

pub use mock::{default_scenario, mock_json, mock_registry, mock_report, MockProvider, Scenario};
pub use types::{
    collect, humanize, severity_for_used_percent, AuthKind, DataSource, FetchStatus, MoneyBalance,
    NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow, Severity, UsageReport,
    WindowKind, SCHEMA_VERSION,
};
