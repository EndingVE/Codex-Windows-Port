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
pub mod refresh;
pub mod types;

pub use mock::{
    default_scenario, mock_json, mock_registry, mock_registry_shared, mock_report, MockProvider,
    Scenario,
};
pub use refresh::{
    classify_error, collect_with, retain_last_good, CollectOptions, CollectResult, FailureClass,
    OutcomeKind, ProviderBackoff, ProviderOutcome, ProviderRefreshStatus, RefreshStatus,
};
pub use types::{
    collect, collect_parallel, humanize, severity_for_used_percent, AuthKind, DataSource,
    FetchStatus, MoneyBalance, NamedRateWindow, Provider, ProviderId, ProviderSnapshot, RateWindow,
    Severity, UsageReport, WindowKind, SCHEMA_VERSION,
};
