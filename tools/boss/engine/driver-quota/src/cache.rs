//! The cache in front of the probes, and the refresh policy it enforces.
//!
//! # Refresh policy
//!
//! - **Lazy.** Nothing probes until something asks for a snapshot. Engine
//!   startup never triggers a cycle, and opening Preferences on a warm cache
//!   does not either.
//! - **TTL [`DEFAULT_TTL`] (15 minutes).** An ordinary read older than this
//!   runs one cycle; anything fresher is served straight from memory. Three
//!   provider calls per quarter-hour of active looking is not hammering.
//! - **Explicit refresh bypasses the TTL but not the floor.** An explicit
//!   refresh is honoured unless a cycle ran within
//!   [`DEFAULT_MIN_REFRESH_INTERVAL`] (60 seconds); a refusal is reported as
//!   `refresh_throttled` on the snapshot, so a held-down button cannot turn
//!   into a request loop and the UI can still say why the timestamp did not
//!   move.
//! - **Reads never wait on a cycle.** [`QuotaCache::lookup`] returns the
//!   cached snapshot immediately and says whether a cycle is due; the RPC
//!   handler replies with that and, if needed, runs [`QuotaCache::snapshot`]
//!   on a background task. Opening Preferences does not stall other app
//!   RPCs behind three child-process probes.
//! - **Concurrent cycles coalesce.** A dedicated mutex serializes
//!   [`QuotaCache::snapshot`] so two windows opening at once produce one set
//!   of probes, not two. The snapshot mutex is not held while probes run.
//!
//! # Not a dispatch slot
//!
//! A cycle is three futures owned by this struct. It allocates no worker
//! slot, creates no execution row, opens no pane, and never consults the
//! dispatcher — the engine's concurrency accounting is derived from execution
//! rows, and this code cannot create one (it does not depend on the crate
//! that can). [`QuotaCache`] holds nothing but probes and a snapshot.

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{
    DRIVER_QUOTA_ORDER, DriverQuotaEntry, DriverQuotaFailureKind, DriverQuotaOutcome, DriverQuotaSnapshot,
};

use crate::{DEFAULT_PROBE_TIMEOUT, DriverQuotaProbe, now_epoch_s};

/// How long a cycle's results are served without re-probing.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// Floor between two probe cycles, even for an explicit refresh.
pub const DEFAULT_MIN_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// The probes a cache drives, one per implemented driver.
pub type QuotaProbeSet = Vec<Arc<dyn DriverQuotaProbe>>;

/// Result of [`QuotaCache::lookup`]: whatever is already cached, plus
/// whether the caller should start a background cycle.
#[derive(Debug, Clone)]
pub struct QuotaLookup {
    pub snapshot: DriverQuotaSnapshot,
    pub should_probe: bool,
}

/// Cached provider quota readings plus the policy governing when to re-probe.
pub struct QuotaCache {
    probes: QuotaProbeSet,
    ttl: Duration,
    min_refresh_interval: Duration,
    probe_timeout: Duration,
    locks: QuotaLocks,
}

/// Snapshot and cycle mutexes, grouped so [`QuotaCache`] stays at five
/// named fields (the giant-structs check) without collapsing the two
/// locks into one — cached reads must not wait on a running cycle.
struct QuotaLocks {
    state: tokio::sync::Mutex<CacheState>,
    cycle: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct CacheState {
    /// Last known figures, merged per driver as a running cycle's probes
    /// land. `generated_at_epoch_s` only moves when the whole cycle ends.
    snapshot: DriverQuotaSnapshot,
    /// Drivers whose probe is running in the current cycle, if any.
    pending: Vec<String>,
}

impl QuotaCache {
    /// Build a cache over the given probes with the default policy.
    pub fn new(probes: QuotaProbeSet) -> Self {
        Self {
            probes,
            ttl: DEFAULT_TTL,
            min_refresh_interval: DEFAULT_MIN_REFRESH_INTERVAL,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            locks: QuotaLocks {
                state: tokio::sync::Mutex::new(CacheState::default()),
                cycle: tokio::sync::Mutex::new(()),
            },
        }
    }

    /// Override the timing policy. Tests use this to avoid sleeping.
    #[cfg(test)]
    pub fn with_policy(mut self, ttl: Duration, min_refresh_interval: Duration, probe_timeout: Duration) -> Self {
        self.ttl = ttl;
        self.min_refresh_interval = min_refresh_interval;
        self.probe_timeout = probe_timeout;
        self
    }

    fn decide(&self, generated_at_epoch_s: Option<i64>, refresh: bool, now: i64) -> (bool, bool) {
        let age_s = generated_at_epoch_s.map(|generated| now.saturating_sub(generated));
        match age_s {
            // Never probed: always run, whatever the flag says.
            None => (true, false),
            Some(age) if refresh => {
                let floor = self.min_refresh_interval.as_secs() as i64;
                if age >= floor { (true, false) } else { (false, true) }
            }
            Some(age) => (age >= self.ttl.as_secs() as i64, false),
        }
    }

    /// Cached snapshot plus whether a probe cycle is due.
    ///
    /// Never runs a probe. The RPC handler replies with `snapshot` on the
    /// request loop and, when `should_probe` is set, runs [`Self::snapshot`]
    /// on a background task so other app RPCs are not stalled.
    pub async fn lookup(&self, refresh: bool) -> QuotaLookup {
        let mut state = self.locks.state.lock().await;
        let (should_probe, throttled) = self.decide(state.snapshot.generated_at_epoch_s, refresh, now_epoch_s());
        // The flag describes *this* request's decision, so it is rewritten on
        // every lookup rather than left holding an earlier decline.
        state.snapshot.refresh_throttled = throttled;
        let mut snapshot = state.snapshot.clone();
        // Only a request that will be followed by pushed results waits on
        // anything; a joined in-flight cycle reports its remaining drivers.
        if should_probe {
            snapshot.pending_drivers = if state.pending.is_empty() {
                self.probes.iter().map(|probe| probe.driver().to_owned()).collect()
            } else {
                state.pending.clone()
            };
        }
        QuotaLookup { snapshot, should_probe }
    }

    /// Return the current snapshot, probing first if policy says to.
    ///
    /// `refresh` marks an explicit refresh request from the UI. See the
    /// module doc for exactly what each flag combination does. The snapshot
    /// mutex is not held while probes run; concurrent callers share one
    /// cycle via the cycle lock. The RPC handler uses [`Self::lookup`] to
    /// reply immediately, then this method on a background task.
    pub async fn snapshot(&self, refresh: bool) -> DriverQuotaSnapshot {
        self.snapshot_with_progress(refresh, |_| {}).await
    }

    /// Like [`Self::snapshot`], but calls `progress` with a partial snapshot
    /// each time one driver's probe completes, so a fast driver's result is
    /// visible without waiting for the slowest. Every partial snapshot carries
    /// the latest known entry for each driver (this cycle's where it has
    /// landed, the previous cycle's otherwise) and lists the still-running
    /// drivers in `pending_drivers`. The final snapshot is returned, not
    /// passed to `progress`.
    pub async fn snapshot_with_progress(
        &self,
        refresh: bool,
        progress: impl Fn(DriverQuotaSnapshot),
    ) -> DriverQuotaSnapshot {
        {
            let mut state = self.locks.state.lock().await;
            let (should_probe, throttled) = self.decide(state.snapshot.generated_at_epoch_s, refresh, now_epoch_s());
            state.snapshot.refresh_throttled = throttled;
            if !should_probe {
                return state.snapshot.clone();
            }
        }
        let _cycle = self.locks.cycle.lock().await;
        {
            let mut state = self.locks.state.lock().await;
            let (should_probe, _) = self.decide(state.snapshot.generated_at_epoch_s, refresh, now_epoch_s());
            if !should_probe {
                state.snapshot.refresh_throttled = false;
                return state.snapshot.clone();
            }
            state.pending = self.probes.iter().map(|probe| probe.driver().to_owned()).collect();
        }
        let new_snapshot = self.run_cycle(&progress).await;
        let mut state = self.locks.state.lock().await;
        state.snapshot = new_snapshot;
        state.pending.clear();
        state.snapshot.clone()
    }

    /// Run every probe concurrently under its own deadline.
    async fn run_cycle(&self, progress: &impl Fn(DriverQuotaSnapshot)) -> DriverQuotaSnapshot {
        let mut join_set = tokio::task::JoinSet::new();
        for probe in &self.probes {
            let probe = Arc::clone(probe);
            let timeout = self.probe_timeout;
            boss_task_scope::spawn_into(&mut join_set, async move {
                let driver = probe.driver().to_owned();
                let outcome = match tokio::time::timeout(timeout, probe.probe()).await {
                    Ok(outcome) => outcome,
                    Err(_) => DriverQuotaOutcome::Unavailable {
                        kind: DriverQuotaFailureKind::Timeout,
                        reason: format!("no answer within {}s", timeout.as_secs()),
                    },
                };
                DriverQuotaEntry {
                    driver,
                    observed_at_epoch_s: now_epoch_s(),
                    outcome,
                }
            });
        }
        let mut entries: Vec<DriverQuotaEntry> = Vec::new();
        while let Some(joined) = join_set.join_next().await {
            match joined {
                Ok(entry) => {
                    let partial = self.record_landed(&entry).await;
                    entries.push(entry);
                    progress(partial);
                }
                // A panicking probe must not take the whole cycle down; it
                // simply contributes nothing, and the driver is then absent
                // from the snapshot — which the UI renders as "no result",
                // not as zero.
                Err(err) => tracing::error!(?err, "driver_quota: probe task failed"),
            }
        }
        sort_for_display(&mut entries);

        for entry in &entries {
            match &entry.outcome {
                DriverQuotaOutcome::Reading(reading) => tracing::debug!(
                    driver = %entry.driver,
                    used_percent = reading.used_percent,
                    source = %reading.source,
                    "driver_quota: reading",
                ),
                DriverQuotaOutcome::Unavailable { kind, reason } => tracing::info!(
                    driver = %entry.driver,
                    kind = kind.as_str(),
                    %reason,
                    "driver_quota: unavailable",
                ),
            }
        }

        DriverQuotaSnapshot {
            entries,
            generated_at_epoch_s: Some(now_epoch_s()),
            refresh_throttled: false,
            pending_drivers: Vec::new(),
        }
    }

    /// Fold one landed probe into the running cycle's state and return the
    /// partial snapshot to publish.
    async fn record_landed(&self, entry: &DriverQuotaEntry) -> DriverQuotaSnapshot {
        let mut state = self.locks.state.lock().await;
        let entries = &mut state.snapshot.entries;
        match entries.iter_mut().find(|existing| existing.driver == entry.driver) {
            Some(existing) => *existing = entry.clone(),
            None => entries.push(entry.clone()),
        }
        sort_for_display(entries);
        state.pending.retain(|driver| *driver != entry.driver);
        let mut partial = state.snapshot.clone();
        partial.refresh_throttled = false;
        partial.pending_drivers = state.pending.clone();
        partial
    }
}

/// Stable presentation order regardless of which probe answered first.
fn sort_for_display(entries: &mut [DriverQuotaEntry]) {
    entries.sort_by_key(|entry| {
        DRIVER_QUOTA_ORDER
            .iter()
            .position(|slug| *slug == entry.driver)
            .unwrap_or(usize::MAX)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use boss_protocol::{DriverQuotaReading, DriverQuotaWindow};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingProbe {
        driver: &'static str,
        calls: AtomicUsize,
        percent: f64,
    }

    impl CountingProbe {
        fn new(driver: &'static str, percent: f64) -> Arc<Self> {
            Arc::new(Self {
                driver,
                calls: AtomicUsize::new(0),
                percent,
            })
        }
    }

    #[async_trait]
    impl DriverQuotaProbe for CountingProbe {
        fn driver(&self) -> &'static str {
            self.driver
        }
        async fn probe(&self) -> DriverQuotaOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            DriverQuotaOutcome::Reading(DriverQuotaReading {
                used_percent: self.percent,
                window: DriverQuotaWindow::Weekly,
                resets_at_epoch_s: None,
                resets_at_text: None,
                source: "test".to_owned(),
            })
        }
    }

    struct SlowProbe;

    #[async_trait]
    impl DriverQuotaProbe for SlowProbe {
        fn driver(&self) -> &'static str {
            "codex"
        }
        async fn probe(&self) -> DriverQuotaOutcome {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!("probe should have been cut off by its deadline")
        }
    }

    fn kind_of(entry: &DriverQuotaEntry) -> Option<DriverQuotaFailureKind> {
        match &entry.outcome {
            DriverQuotaOutcome::Unavailable { kind, .. } => Some(*kind),
            DriverQuotaOutcome::Reading(_) => None,
        }
    }

    #[tokio::test]
    async fn first_read_probes_and_second_read_within_ttl_does_not() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache = QuotaCache::new(vec![probe.clone()]);

        let first = cache.snapshot(false).await;
        assert_eq!(first.entries.len(), 1);
        assert!(first.generated_at_epoch_s.is_some());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);

        let second = cache.snapshot(false).await;
        assert_eq!(second.entries.len(), 1);
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            1,
            "opening Preferences again must not re-probe inside the TTL"
        );
    }

    #[tokio::test]
    async fn expired_ttl_triggers_exactly_one_new_cycle() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache =
            QuotaCache::new(vec![probe.clone()]).with_policy(Duration::ZERO, Duration::ZERO, DEFAULT_PROBE_TIMEOUT);
        cache.snapshot(false).await;
        cache.snapshot(false).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn explicit_refresh_inside_the_floor_is_throttled_not_silently_ignored() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache = QuotaCache::new(vec![probe.clone()]).with_policy(
            DEFAULT_TTL,
            Duration::from_secs(3600),
            DEFAULT_PROBE_TIMEOUT,
        );
        let first = cache.snapshot(false).await;
        assert!(!first.refresh_throttled);

        let second = cache.snapshot(true).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1, "floor must hold");
        assert!(
            second.refresh_throttled,
            "a declined refresh must be visible, not a frozen timestamp"
        );
        assert_eq!(second.generated_at_epoch_s, first.generated_at_epoch_s);
    }

    #[tokio::test]
    async fn explicit_refresh_past_the_floor_reprobes_even_inside_the_ttl() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache = QuotaCache::new(vec![probe.clone()]).with_policy(
            Duration::from_secs(3600),
            Duration::ZERO,
            DEFAULT_PROBE_TIMEOUT,
        );
        cache.snapshot(false).await;
        let refreshed = cache.snapshot(true).await;
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        assert!(!refreshed.refresh_throttled);
    }

    #[tokio::test]
    async fn a_hung_probe_becomes_a_timeout_entry_and_does_not_block_the_others() {
        let claude = CountingProbe::new("claude", 1.0);
        let cache = QuotaCache::new(vec![claude, Arc::new(SlowProbe)]).with_policy(
            DEFAULT_TTL,
            DEFAULT_MIN_REFRESH_INTERVAL,
            Duration::from_millis(50),
        );
        let snapshot = cache.snapshot(false).await;
        assert_eq!(snapshot.entries.len(), 2);
        let codex = snapshot
            .entries
            .iter()
            .find(|e| e.driver == "codex")
            .expect("codex entry present");
        assert_eq!(kind_of(codex), Some(DriverQuotaFailureKind::Timeout));
        assert!(
            matches!(
                snapshot
                    .entries
                    .iter()
                    .find(|e| e.driver == "claude")
                    .map(|e| &e.outcome),
                Some(DriverQuotaOutcome::Reading(_))
            ),
            "a hung driver must not suppress a healthy one",
        );
    }

    #[tokio::test]
    async fn entries_are_ordered_for_display_not_by_completion() {
        let cache = QuotaCache::new(vec![
            CountingProbe::new("grok", 1.0),
            CountingProbe::new("claude", 2.0),
            CountingProbe::new("codex", 3.0),
        ]);
        let snapshot = cache.snapshot(false).await;
        let order: Vec<&str> = snapshot.entries.iter().map(|e| e.driver.as_str()).collect();
        assert_eq!(order, DRIVER_QUOTA_ORDER.to_vec());
    }

    #[tokio::test]
    async fn lookup_does_not_probe_and_reports_when_a_cycle_is_due() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache = QuotaCache::new(vec![probe.clone()]);
        let first = cache.lookup(false).await;
        assert!(first.should_probe);
        assert!(first.snapshot.entries.is_empty());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);

        cache.snapshot(false).await;
        let warm = cache.lookup(false).await;
        assert!(!warm.should_probe);
        assert_eq!(warm.snapshot.entries.len(), 1);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn concurrent_readers_share_one_cycle() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache = Arc::new(QuotaCache::new(vec![probe.clone()]));
        let a = {
            let c = Arc::clone(&cache);
            tokio::spawn(async move { c.snapshot(false).await })
        };
        let b = {
            let c = Arc::clone(&cache);
            tokio::spawn(async move { c.snapshot(false).await })
        };
        let (a, b) = (a.await.expect("join"), b.await.expect("join"));
        assert_eq!(a.entries.len(), 1);
        assert_eq!(b.entries.len(), 1);
        assert_eq!(
            probe.calls.load(Ordering::SeqCst),
            1,
            "two windows opening at once must not double-probe the providers"
        );
    }

    struct GatedProbe {
        driver: &'static str,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl DriverQuotaProbe for GatedProbe {
        fn driver(&self) -> &'static str {
            self.driver
        }
        async fn probe(&self) -> DriverQuotaOutcome {
            self.gate.notified().await;
            DriverQuotaOutcome::Reading(DriverQuotaReading {
                used_percent: 9.0,
                window: DriverQuotaWindow::Weekly,
                resets_at_epoch_s: None,
                resets_at_text: None,
                source: "test".to_owned(),
            })
        }
    }

    #[tokio::test]
    async fn a_fast_driver_is_published_while_a_slow_one_is_still_pending() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let cache = Arc::new(QuotaCache::new(vec![
            CountingProbe::new("claude", 1.0),
            Arc::new(GatedProbe {
                driver: "codex",
                gate: Arc::clone(&gate),
            }),
        ]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cycle = {
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                cache
                    .snapshot_with_progress(false, move |partial| {
                        let _ = tx.send(partial);
                    })
                    .await
            })
        };
        let partial = rx.recv().await.expect("claude lands first");
        assert_eq!(partial.entries.len(), 1);
        assert_eq!(partial.entries[0].driver, "claude");
        assert_eq!(partial.pending_drivers, vec!["codex".to_owned()]);
        assert!(partial.generated_at_epoch_s.is_none(), "the cycle has not finished");

        let mid = cache.lookup(false).await;
        assert!(mid.should_probe);
        assert_eq!(mid.snapshot.pending_drivers, vec!["codex".to_owned()]);

        gate.notify_one();
        let done = cycle.await.expect("join");
        assert_eq!(done.entries.len(), 2);
        assert!(done.pending_drivers.is_empty());
        assert!(done.generated_at_epoch_s.is_some());
    }

    #[tokio::test]
    async fn an_accepted_refresh_after_a_declined_one_is_not_reported_throttled() {
        let probe = CountingProbe::new("claude", 7.0);
        let cache = QuotaCache::new(vec![probe.clone()]).with_policy(
            DEFAULT_TTL,
            Duration::from_secs(3600),
            DEFAULT_PROBE_TIMEOUT,
        );
        cache.snapshot(false).await;
        let declined = cache.lookup(true).await;
        assert!(declined.snapshot.refresh_throttled);
        assert!(!declined.should_probe);
        assert!(declined.snapshot.pending_drivers.is_empty());

        // Age the cached cycle past the floor instead of sleeping through it.
        {
            let mut state = cache.locks.state.lock().await;
            let generated = state.snapshot.generated_at_epoch_s.expect("cycle ran");
            state.snapshot.generated_at_epoch_s = Some(generated - 7200);
        }
        let accepted = cache.lookup(true).await;
        assert!(accepted.should_probe);
        assert!(
            !accepted.snapshot.refresh_throttled,
            "a stale decline must not describe the accepted request"
        );
        assert_eq!(accepted.snapshot.pending_drivers, vec!["claude".to_owned()]);
    }
}
