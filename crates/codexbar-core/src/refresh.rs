//! Refresh pipeline primitives: parallel collection with a concurrency cap and a
//! per-provider deadline, failure classification, "last known good" retention
//! and per-provider backoff.
//!
//! This module is the **public error/result API** other layers build on:
//!
//! * [`collect_with`] fetches every provider on worker threads and returns one
//!   [`ProviderOutcome`] per provider (kind + elapsed + snapshot), in canonical
//!   order. A provider that hangs past [`CollectOptions::provider_timeout`] is
//!   reported as [`OutcomeKind::TimedOut`] and abandoned; it never blocks the
//!   others.
//! * [`FailureClass`] / [`classify_error`] turn an error message into a coarse,
//!   UI-actionable class (auth, rate-limited, network, timeout, server, other).
//!   `codexbar_providers::HttpError::class()` maps structured HTTP errors onto
//!   the same enum.
//! * [`retain_last_good`] keeps the previous numbers — marked
//!   [`FetchStatus::Stale`] — when a fetch failed for a transient reason or the
//!   machine is offline. Auth failures are **never** masked: the user must see
//!   the re-authentication prompt.
//! * [`ProviderBackoff`] spaces out retries of a provider that keeps failing,
//!   so a dead endpoint is not hammered on every tick.
//! * [`RefreshStatus`] is the serialisable per-provider summary of a cycle
//!   (outcome, failure class, stale flag, backoff countdown) for the UI.
//!
//! Nothing here performs I/O itself; the HTTP layer lives in
//! `codexbar-providers`.

use std::collections::HashMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::types::{DataSource, FetchStatus, Provider, ProviderId, ProviderSnapshot, UsageReport};

/// Default number of providers fetched at the same time.
pub const DEFAULT_MAX_CONCURRENCY: usize = 6;

/// Default wall-clock budget for one provider's `fetch` (all of its requests
/// and retries). Providers use request timeouts of 4-30 s and the HTTP retry
/// layer stops starting new attempts after 25 s; this is the outer fence that
/// keeps one wedged provider from holding up the report.
pub const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(60);

/// Tuning for [`collect_with`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollectOptions {
    /// Maximum providers in flight at once (at least 1).
    pub max_concurrency: usize,
    /// Deadline for a single provider, measured from the moment it starts.
    pub provider_timeout: Duration,
}

impl Default for CollectOptions {
    fn default() -> Self {
        Self {
            max_concurrency: DEFAULT_MAX_CONCURRENCY,
            provider_timeout: DEFAULT_PROVIDER_TIMEOUT,
        }
    }
}

/// Coarse, user-actionable reason a fetch failed.
///
/// Serialised in camelCase so a UI can switch on it directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FailureClass {
    /// Credentials rejected or expired (401/403, `invalid_grant`, …). Needs the
    /// user to sign in again — never retried, never hidden behind stale data.
    Auth,
    /// 429 / quota throttling. Transient.
    RateLimited,
    /// DNS, TLS, refused connection, no route. Transient.
    Network,
    /// The request (or the whole provider) exceeded its deadline. Transient.
    Timeout,
    /// 5xx from the provider. Transient.
    Server,
    /// Anything else (decode error, unexpected payload, bad config).
    Other,
}

impl FailureClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            FailureClass::Auth => "auth",
            FailureClass::RateLimited => "rateLimited",
            FailureClass::Network => "network",
            FailureClass::Timeout => "timeout",
            FailureClass::Server => "server",
            FailureClass::Other => "other",
        }
    }

    /// True for failures worth retrying later (and worth masking with the last
    /// known good numbers in the meantime).
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            FailureClass::RateLimited
                | FailureClass::Network
                | FailureClass::Timeout
                | FailureClass::Server
        )
    }
}

/// Classify a (redacted) error message.
///
/// Providers report failures as prose in `ProviderSnapshot::error`; this is the
/// single place that maps that prose onto a [`FailureClass`]. Auth wins over
/// everything else so a "401 … timed out" style message still prompts a
/// re-login.
pub fn classify_error(message: &str) -> FailureClass {
    let text = message.to_ascii_lowercase();
    let has_status = |code: &str| {
        text.contains(&format!("http {code}"))
            || text.contains(&format!("status {code}"))
            || text.contains(&format!("({code})"))
            || text.contains(&format!("{code} "))
                && (text.contains("http") || text.contains("status"))
    };

    const AUTH: [&str; 12] = [
        "unauthorized",
        "forbidden",
        "invalid_grant",
        "token expired",
        "token is expired",
        "expired token",
        "token revoked",
        "re-authenticate",
        "reauth",
        "sign in again",
        "log in again",
        "invalid api key",
    ];
    if AUTH.iter().any(|n| text.contains(n)) || has_status("401") || has_status("403") {
        return FailureClass::Auth;
    }
    if has_status("429") || text.contains("rate limit") || text.contains("too many requests") {
        return FailureClass::RateLimited;
    }
    if text.contains("timed out") || text.contains("timeout") || text.contains("deadline") {
        return FailureClass::Timeout;
    }
    const NETWORK: [&str; 9] = [
        "could not be reached",
        "connection refused",
        "connection reset",
        "dns",
        "no such host",
        "network is unreachable",
        "error sending request",
        "offline",
        "tls",
    ];
    if NETWORK.iter().any(|n| text.contains(n)) {
        return FailureClass::Network;
    }
    if ["500", "502", "503", "504"].iter().any(|c| has_status(c)) {
        return FailureClass::Server;
    }
    FailureClass::Other
}

/// What happened to one provider during a collection pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OutcomeKind {
    /// `fetch` returned (whatever status it reported).
    Completed,
    /// `fetch` did not return before the deadline; the thread was abandoned.
    TimedOut,
    /// `fetch` panicked (only observable with `panic = "unwind"`).
    Panicked,
    /// Not fetched this cycle (backoff); the snapshot is carried over.
    Skipped,
}

/// Per-provider result of a refresh cycle.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderOutcome {
    pub provider: ProviderId,
    pub kind: OutcomeKind,
    /// Wall-clock time spent on this provider (zero when skipped).
    pub elapsed: Duration,
    /// The snapshot the report carries for this provider.
    pub snapshot: ProviderSnapshot,
}

impl ProviderOutcome {
    /// `Some(class)` when the provider ended the cycle in a failed state.
    pub fn failure(&self) -> Option<FailureClass> {
        match self.kind {
            OutcomeKind::TimedOut => Some(FailureClass::Timeout),
            OutcomeKind::Panicked => Some(FailureClass::Other),
            OutcomeKind::Skipped => None,
            OutcomeKind::Completed => match self.snapshot.status {
                FetchStatus::Error => Some(classify_error(
                    self.snapshot.error.as_deref().unwrap_or_default(),
                )),
                _ => None,
            },
        }
    }
}

/// Result of [`collect_with`].
#[derive(Debug, Clone)]
pub struct CollectResult {
    pub report: UsageReport,
    /// One entry per provider, same order as `report.providers`.
    pub outcomes: Vec<ProviderOutcome>,
}

fn canonical_index(id: ProviderId) -> usize {
    ProviderId::ALL
        .iter()
        .position(|p| *p == id)
        .unwrap_or(usize::MAX)
}

/// Error snapshot used when the pipeline itself gives up on a provider.
pub fn failed_snapshot(
    id: ProviderId,
    message: impl Into<String>,
    now: DateTime<Utc>,
) -> ProviderSnapshot {
    ProviderSnapshot {
        provider: id,
        title: id.title().to_string(),
        account: None,
        plan: None,
        windows: Vec::new(),
        balance: None,
        status: FetchStatus::Error,
        error: Some(message.into()),
        source: id.auth_kind_source(),
        fetched_at: now,
    }
}

/// Fetch `providers` concurrently (at most `options.max_concurrency` at a time),
/// giving each at most `options.provider_timeout`.
///
/// The providers are `Arc`s so a wedged fetch can be abandoned on its thread
/// without borrowing from the caller. `skip` lets the caller leave some
/// providers out of this cycle (backoff); they are returned as
/// [`OutcomeKind::Skipped`] with the snapshot `carry` produced.
///
/// Output is always in canonical [`ProviderId::ALL`] order.
pub fn collect_with(
    providers: &[Arc<dyn Provider>],
    options: CollectOptions,
    now: DateTime<Utc>,
    skip: &dyn Fn(ProviderId) -> Option<ProviderSnapshot>,
) -> CollectResult {
    let limit = options.max_concurrency.max(1);
    let (tx, rx) = mpsc::channel::<(usize, thread::Result<ProviderSnapshot>)>();

    let mut results: Vec<Option<ProviderOutcome>> = vec![None; providers.len()];
    let mut queue: Vec<usize> = Vec::new();
    for (index, provider) in providers.iter().enumerate() {
        match skip(provider.id()) {
            Some(snapshot) => {
                results[index] = Some(ProviderOutcome {
                    provider: provider.id(),
                    kind: OutcomeKind::Skipped,
                    elapsed: Duration::ZERO,
                    snapshot,
                });
            }
            None => queue.push(index),
        }
    }
    queue.reverse(); // pop() from the back keeps registry order

    // index -> start instant
    let mut in_flight: HashMap<usize, Instant> = HashMap::new();

    loop {
        while in_flight.len() < limit {
            let Some(index) = queue.pop() else { break };
            let provider = Arc::clone(&providers[index]);
            let tx = tx.clone();
            in_flight.insert(index, Instant::now());
            let spawned = thread::Builder::new()
                .name(format!("codexbar-fetch-{}", provider.id().as_str()))
                .spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        provider.fetch(now)
                    }));
                    // The collector may have given up on us; that is fine.
                    let _ = tx.send((index, result));
                });
            if let Err(err) = spawned {
                in_flight.remove(&index);
                let id = providers[index].id();
                results[index] = Some(ProviderOutcome {
                    provider: id,
                    kind: OutcomeKind::Panicked,
                    elapsed: Duration::ZERO,
                    snapshot: failed_snapshot(id, format!("could not start fetch: {err}"), now),
                });
            }
        }

        if in_flight.is_empty() {
            break;
        }

        let earliest = in_flight
            .values()
            .map(|start| *start + options.provider_timeout)
            .min()
            .expect("in_flight is not empty");
        let wait = earliest.saturating_duration_since(Instant::now());

        match rx.recv_timeout(wait) {
            Ok((index, result)) => {
                let Some(start) = in_flight.remove(&index) else {
                    continue; // late answer from an abandoned provider
                };
                let id = providers[index].id();
                let (kind, snapshot) = match result {
                    Ok(snapshot) => (OutcomeKind::Completed, snapshot),
                    Err(_) => (
                        OutcomeKind::Panicked,
                        failed_snapshot(id, "provider fetch panicked", now),
                    ),
                };
                results[index] = Some(ProviderOutcome {
                    provider: id,
                    kind,
                    elapsed: start.elapsed(),
                    snapshot,
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now_instant = Instant::now();
                let expired: Vec<usize> = in_flight
                    .iter()
                    .filter(|(_, start)| now_instant >= **start + options.provider_timeout)
                    .map(|(index, _)| *index)
                    .collect();
                for index in expired {
                    let start = in_flight.remove(&index).expect("present");
                    let id = providers[index].id();
                    results[index] = Some(ProviderOutcome {
                        provider: id,
                        kind: OutcomeKind::TimedOut,
                        elapsed: start.elapsed(),
                        snapshot: failed_snapshot(
                            id,
                            format!(
                                "{} did not answer within {}s (timed out)",
                                id.title(),
                                options.provider_timeout.as_secs()
                            ),
                            now,
                        ),
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    let mut outcomes: Vec<ProviderOutcome> = results.into_iter().flatten().collect();
    outcomes.sort_by_key(|o| canonical_index(o.provider));
    let report = UsageReport::new(outcomes.iter().map(|o| o.snapshot.clone()).collect());
    CollectResult { report, outcomes }
}

/// Carry the previous good numbers forward when a fetch failed transiently.
///
/// Rules, per provider:
/// * the fresh snapshot is `Ok` / `NotConfigured` → take it as is;
/// * it failed with an **auth** failure → take it as is (the re-login prompt
///   must be visible);
/// * it failed transiently, or `offline` is true, and the previous snapshot had
///   usable numbers (`Ok` or `Stale` with at least one window) → keep the
///   previous numbers and `fetched_at`, set status `Stale`, and put the reason
///   in `error`.
pub fn retain_last_good(
    previous: Option<&UsageReport>,
    fresh: ProviderSnapshot,
    offline: bool,
) -> ProviderSnapshot {
    if fresh.status != FetchStatus::Error {
        return fresh;
    }
    let reason = fresh.error.clone().unwrap_or_default();
    let class = classify_error(&reason);
    if class == FailureClass::Auth || !(class.is_transient() || offline) {
        return fresh;
    }
    let Some(prev) = previous.and_then(|r| r.get(fresh.provider)) else {
        return fresh;
    };
    // Never dress sample data up as a live provider's last known values (the
    // user just switched mock mode off).
    if prev.source == DataSource::Mock && fresh.source != DataSource::Mock {
        return fresh;
    }
    let usable =
        matches!(prev.status, FetchStatus::Ok | FetchStatus::Stale) && !prev.windows.is_empty();
    if !usable {
        return fresh;
    }
    let mut kept = prev.clone();
    kept.status = FetchStatus::Stale;
    kept.error = Some(if offline {
        "Offline — showing the last known values.".to_string()
    } else {
        format!("Showing the last known values: {reason}")
    });
    kept
}

/// Exponential per-provider backoff for repeated failures.
///
/// After the first failure the provider is retried on the next tick as usual;
/// from the second consecutive failure on it waits `base * 2^(n-2)` (capped at
/// `max`) before it is fetched again. A success resets it. A manual refresh
/// ignores the backoff (see [`ProviderBackoff::should_skip`]).
#[derive(Debug, Clone)]
pub struct ProviderBackoff {
    base: Duration,
    max: Duration,
    state: HashMap<ProviderId, (u32, Instant)>,
}

impl ProviderBackoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self {
            base,
            max,
            state: HashMap::new(),
        }
    }

    /// Delay imposed after `failures` consecutive failures.
    pub fn delay_for(&self, failures: u32) -> Duration {
        if failures < 2 {
            return Duration::ZERO;
        }
        let exp = (failures - 2).min(16);
        self.base.saturating_mul(1u32 << exp).min(self.max)
    }

    /// Consecutive failures recorded for `id`.
    pub fn failures(&self, id: ProviderId) -> u32 {
        self.state.get(&id).map(|(n, _)| *n).unwrap_or(0)
    }

    /// True when `id` is still inside its backoff window at `now`.
    pub fn should_skip(&self, id: ProviderId, now: Instant) -> bool {
        match self.state.get(&id) {
            Some((failures, since)) => now < *since + self.delay_for(*failures),
            None => false,
        }
    }

    /// Record the outcome of a fetch. Only *transient* failures back off: an
    /// auth failure keeps being checked every tick so a re-login is picked up.
    pub fn record(&mut self, id: ProviderId, failure: Option<FailureClass>, now: Instant) {
        match failure {
            Some(class) if class.is_transient() => {
                let failures = self.failures(id).saturating_add(1);
                self.state.insert(id, (failures, now));
            }
            _ => {
                self.state.remove(&id);
            }
        }
    }

    /// Time left before `id` is fetched again, when it is backing off.
    pub fn retry_in(&self, id: ProviderId, now: Instant) -> Option<Duration> {
        let (failures, since) = self.state.get(&id)?;
        let until = *since + self.delay_for(*failures);
        (until > now).then(|| until - now)
    }

    pub fn reset(&mut self) {
        self.state.clear();
    }
}

/// Serializable per-provider view of the last refresh cycle — what a UI (or
/// the S5 error UX) reads to explain *why* a card is red or stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRefreshStatus {
    pub provider: ProviderId,
    pub outcome: OutcomeKind,
    /// Present when the provider ended the cycle failed (before stale masking).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureClass>,
    pub elapsed_ms: u64,
    /// True when the report shows the last known values instead of fresh ones.
    pub stale: bool,
    pub consecutive_failures: u32,
    /// Seconds until the provider is fetched again when it is backing off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_secs: Option<u64>,
}

/// Serializable summary of the last refresh cycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshStatus {
    /// The machine looked offline this cycle (no HTTP response at all, network
    /// failures only). Cards keep their last known values, marked stale.
    pub offline: bool,
    pub finished_at: DateTime<Utc>,
    pub providers: Vec<ProviderRefreshStatus>,
}

impl RefreshStatus {
    /// Build the summary from a cycle's outcomes, the report actually published
    /// (after [`retain_last_good`]) and the backoff state after recording.
    pub fn from_outcomes(
        outcomes: &[ProviderOutcome],
        published: &UsageReport,
        offline: bool,
        backoff: &ProviderBackoff,
        now: Instant,
    ) -> Self {
        let providers = outcomes
            .iter()
            .map(|o| ProviderRefreshStatus {
                provider: o.provider,
                outcome: o.kind,
                failure: o.failure(),
                elapsed_ms: o.elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
                stale: published
                    .get(o.provider)
                    .is_some_and(|s| s.status == FetchStatus::Stale),
                consecutive_failures: backoff.failures(o.provider),
                retry_in_secs: backoff
                    .retry_in(o.provider, now)
                    .map(|d| d.as_secs().max(1)),
            })
            .collect();
        Self {
            offline,
            finished_at: Utc::now(),
            providers,
        }
    }
}

impl Default for ProviderBackoff {
    fn default() -> Self {
        Self::new(Duration::from_secs(60), Duration::from_secs(30 * 60))
    }
}

/// Whole-machine connectivity verdict for one cycle.
///
/// Offline means: at least one request failed at the network level and not a
/// single HTTP response (of any status) came back. One reachable endpoint is
/// proof the machine is online, even if every provider answered 401.
pub const fn looks_offline(responses: u64, network_failures: u64) -> bool {
    responses == 0 && network_failures > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;
    use crate::types::{NamedRateWindow, RateWindow, WindowKind};

    struct Scripted {
        id: ProviderId,
        delay: Duration,
        error: Option<&'static str>,
    }

    impl Provider for Scripted {
        fn id(&self) -> ProviderId {
            self.id
        }
        fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
            thread::sleep(self.delay);
            match self.error {
                Some(err) => failed_snapshot(self.id, err, now),
                None => ok_snapshot(self.id, 40.0, now),
            }
        }
    }

    fn ok_snapshot(id: ProviderId, used: f64, now: DateTime<Utc>) -> ProviderSnapshot {
        ProviderSnapshot {
            provider: id,
            title: id.title().to_string(),
            account: None,
            plan: None,
            windows: vec![NamedRateWindow::new(
                "session",
                "Session",
                WindowKind::Session,
                RateWindow::new(used, Some(300), None),
            )],
            balance: None,
            status: FetchStatus::Ok,
            error: None,
            source: id.auth_kind_source(),
            fetched_at: now,
        }
    }

    fn scripted(id: ProviderId, ms: u64, error: Option<&'static str>) -> Arc<dyn Provider> {
        Arc::new(Scripted {
            id,
            delay: Duration::from_millis(ms),
            error,
        })
    }

    fn no_skip(_: ProviderId) -> Option<ProviderSnapshot> {
        None
    }

    #[test]
    fn parallel_collect_keeps_canonical_order() {
        // Registered in reverse order with decreasing delays so completion order
        // is the opposite of canonical order.
        let providers: Vec<Arc<dyn Provider>> = ProviderId::ALL
            .iter()
            .rev()
            .enumerate()
            .map(|(i, id)| scripted(*id, (i as u64 % 4) * 5, None))
            .collect();
        let result = collect_with(&providers, CollectOptions::default(), Utc::now(), &no_skip);
        let ids: Vec<ProviderId> = result.report.providers.iter().map(|p| p.provider).collect();
        assert_eq!(ids, ProviderId::ALL.to_vec());
        let outcome_ids: Vec<ProviderId> = result.outcomes.iter().map(|o| o.provider).collect();
        assert_eq!(outcome_ids, ids);
    }

    #[test]
    fn slow_provider_times_out_without_blocking_the_others() {
        let providers = vec![
            scripted(ProviderId::Codex, 5_000, None),
            scripted(ProviderId::Claude, 5, None),
            scripted(ProviderId::Cursor, 5, None),
        ];
        let options = CollectOptions {
            max_concurrency: 2,
            provider_timeout: Duration::from_millis(300),
        };
        let started = Instant::now();
        let result = collect_with(&providers, options, Utc::now(), &no_skip);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "must not wait for the slow one"
        );
        let codex = &result.outcomes[0];
        assert_eq!(codex.provider, ProviderId::Codex);
        assert_eq!(codex.kind, OutcomeKind::TimedOut);
        assert_eq!(codex.snapshot.status, FetchStatus::Error);
        assert_eq!(codex.failure(), Some(FailureClass::Timeout));
        for outcome in &result.outcomes[1..] {
            assert_eq!(outcome.kind, OutcomeKind::Completed);
            assert_eq!(outcome.snapshot.status, FetchStatus::Ok);
        }
    }

    #[test]
    fn concurrency_limit_is_respected() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counting {
            id: ProviderId,
            live: Arc<AtomicUsize>,
            peak: Arc<AtomicUsize>,
        }
        impl Provider for Counting {
            fn id(&self) -> ProviderId {
                self.id
            }
            fn fetch(&self, now: DateTime<Utc>) -> ProviderSnapshot {
                let n = self.live.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(n, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(20));
                self.live.fetch_sub(1, Ordering::SeqCst);
                ok_snapshot(self.id, 1.0, now)
            }
        }
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let providers: Vec<Arc<dyn Provider>> = ProviderId::ALL
            .iter()
            .map(|id| {
                Arc::new(Counting {
                    id: *id,
                    live: Arc::clone(&live),
                    peak: Arc::clone(&peak),
                }) as Arc<dyn Provider>
            })
            .collect();
        let options = CollectOptions {
            max_concurrency: 3,
            provider_timeout: Duration::from_secs(10),
        };
        let result = collect_with(&providers, options, Utc::now(), &no_skip);
        assert_eq!(result.outcomes.len(), ProviderId::ALL.len());
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert!(
            peak.load(Ordering::SeqCst) >= 2,
            "should actually run in parallel"
        );
    }

    #[test]
    fn skipped_providers_are_not_fetched() {
        let providers = vec![
            scripted(ProviderId::Codex, 0, Some("must not run")),
            scripted(ProviderId::Claude, 0, None),
        ];
        let now = Utc::now();
        let carry = ok_snapshot(ProviderId::Codex, 12.0, now);
        let skip = |id: ProviderId| (id == ProviderId::Codex).then(|| carry.clone());
        let result = collect_with(&providers, CollectOptions::default(), now, &skip);
        assert_eq!(result.outcomes[0].kind, OutcomeKind::Skipped);
        assert_eq!(result.outcomes[0].snapshot, carry);
        assert_eq!(result.outcomes[0].failure(), None);
        assert_eq!(result.outcomes[1].kind, OutcomeKind::Completed);
    }

    #[test]
    fn mock_registry_collects_the_same_way() {
        let providers: Vec<Arc<dyn Provider>> = ProviderId::ALL
            .iter()
            .map(|id| Arc::new(MockProvider::new(*id)) as Arc<dyn Provider>)
            .collect();
        let result = collect_with(&providers, CollectOptions::default(), Utc::now(), &no_skip);
        assert_eq!(result.report.providers.len(), ProviderId::ALL.len());
    }

    #[test]
    fn classify_error_buckets() {
        assert_eq!(
            classify_error("HTTP 401 from https://x — nope"),
            FailureClass::Auth
        );
        assert_eq!(
            classify_error("HTTP 403 from https://x"),
            FailureClass::Auth
        );
        assert_eq!(
            classify_error("refresh failed: invalid_grant (token revoked)"),
            FailureClass::Auth
        );
        assert_eq!(
            classify_error("HTTP 429 from https://x — slow down"),
            FailureClass::RateLimited
        );
        assert_eq!(
            classify_error("https://x timed out after 15s"),
            FailureClass::Timeout
        );
        assert_eq!(
            classify_error("https://x could not be reached: dns error"),
            FailureClass::Network
        );
        assert_eq!(
            classify_error("HTTP 503 from https://x — maintenance"),
            FailureClass::Server
        );
        assert_eq!(
            classify_error("usage payload missing field"),
            FailureClass::Other
        );
        assert!(FailureClass::Server.is_transient());
        assert!(!FailureClass::Auth.is_transient());
        assert_eq!(
            serde_json::to_string(&FailureClass::RateLimited).unwrap(),
            "\"rateLimited\""
        );
    }

    #[test]
    fn transient_failure_keeps_last_good_as_stale() {
        let then = Utc::now() - chrono::Duration::minutes(10);
        let previous = UsageReport::new(vec![ok_snapshot(ProviderId::Codex, 55.0, then)]);
        let fresh = failed_snapshot(
            ProviderId::Codex,
            "https://x timed out after 15s",
            Utc::now(),
        );
        let kept = retain_last_good(Some(&previous), fresh, false);
        assert_eq!(kept.status, FetchStatus::Stale);
        assert_eq!(kept.windows.len(), 1);
        assert_eq!(kept.fetched_at, then);
        assert!(kept.error.unwrap().contains("timed out"));
    }

    #[test]
    fn offline_keeps_last_good_even_for_unclassified_errors() {
        let previous = UsageReport::new(vec![ok_snapshot(ProviderId::Claude, 5.0, Utc::now())]);
        let fresh = failed_snapshot(ProviderId::Claude, "something odd", Utc::now());
        let kept = retain_last_good(Some(&previous), fresh, true);
        assert_eq!(kept.status, FetchStatus::Stale);
        assert!(kept.error.unwrap().starts_with("Offline"));
    }

    #[test]
    fn auth_failure_is_never_masked() {
        let previous = UsageReport::new(vec![ok_snapshot(ProviderId::Claude, 5.0, Utc::now())]);
        let fresh = failed_snapshot(ProviderId::Claude, "HTTP 401 from https://x", Utc::now());
        let out = retain_last_good(Some(&previous), fresh.clone(), true);
        assert_eq!(out, fresh);
    }

    #[test]
    fn no_previous_numbers_means_the_error_shows() {
        let fresh = failed_snapshot(ProviderId::Claude, "timed out", Utc::now());
        assert_eq!(retain_last_good(None, fresh.clone(), true), fresh);
    }

    #[test]
    fn backoff_grows_exponentially_and_resets() {
        let mut backoff = ProviderBackoff::new(Duration::from_secs(10), Duration::from_secs(60));
        let t0 = Instant::now();
        let id = ProviderId::Groq;
        backoff.record(id, Some(FailureClass::Network), t0);
        assert!(
            !backoff.should_skip(id, t0),
            "first failure: retry next tick"
        );
        backoff.record(id, Some(FailureClass::Network), t0);
        assert!(backoff.should_skip(id, t0 + Duration::from_secs(5)));
        assert!(!backoff.should_skip(id, t0 + Duration::from_secs(10)));
        assert_eq!(backoff.delay_for(3), Duration::from_secs(20));
        assert_eq!(backoff.delay_for(10), Duration::from_secs(60), "capped");
        backoff.record(id, None, t0);
        assert_eq!(backoff.failures(id), 0);
        // Auth failures do not back off.
        backoff.record(id, Some(FailureClass::Auth), t0);
        backoff.record(id, Some(FailureClass::Auth), t0);
        assert!(!backoff.should_skip(id, t0));
    }

    #[test]
    fn mock_numbers_are_never_carried_into_live_mode() {
        let mut prev = ok_snapshot(ProviderId::Codex, 5.0, Utc::now());
        prev.source = DataSource::Mock;
        let previous = UsageReport::new(vec![prev]);
        let fresh = failed_snapshot(ProviderId::Codex, "timed out", Utc::now());
        assert_eq!(
            retain_last_good(Some(&previous), fresh.clone(), true),
            fresh
        );
    }

    #[test]
    fn refresh_status_serialises_for_the_ui() {
        let now = Utc::now();
        let outcomes = vec![
            ProviderOutcome {
                provider: ProviderId::Codex,
                kind: OutcomeKind::Completed,
                elapsed: Duration::from_millis(120),
                snapshot: failed_snapshot(ProviderId::Codex, "HTTP 503 from https://x", now),
            },
            ProviderOutcome {
                provider: ProviderId::Claude,
                kind: OutcomeKind::Completed,
                elapsed: Duration::from_millis(80),
                snapshot: ok_snapshot(ProviderId::Claude, 3.0, now),
            },
        ];
        let mut backoff = ProviderBackoff::new(Duration::from_secs(60), Duration::from_secs(600));
        let t = Instant::now();
        backoff.record(ProviderId::Codex, Some(FailureClass::Server), t);
        backoff.record(ProviderId::Codex, Some(FailureClass::Server), t);
        let published = UsageReport::new(outcomes.iter().map(|o| o.snapshot.clone()).collect());
        let status = RefreshStatus::from_outcomes(&outcomes, &published, false, &backoff, t);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["offline"], false);
        assert_eq!(json["providers"][0]["failure"], "server");
        assert_eq!(json["providers"][0]["consecutiveFailures"], 2);
        assert_eq!(json["providers"][0]["retryInSecs"], 60);
        assert_eq!(json["providers"][1].get("failure"), None);
        assert_eq!(json["providers"][1]["outcome"], "completed");
    }

    #[test]
    fn offline_verdict() {
        assert!(looks_offline(0, 3));
        assert!(!looks_offline(1, 3));
        assert!(!looks_offline(0, 0));
    }
}
