//! Consecutive pre-start failure tracking and alert notifications.

use crate::{WorkerKind, worker_kind_for_execution};
use boss_protocol::{ExecutionKind, SpawnFailureStreak};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Consecutive pre-start failures, with no success in between, at which a
/// (driver, worker kind) combination raises its alert.
///
/// # Why the threshold is 2
///
/// A pre-start failure is the engine failing to *launch* a worker — a
/// rejected config, a refused gate, a prompt that will not compose. Those
/// are deterministic far more often than they are transient, and the one
/// common benign cause (a `SlotBusy` engine/app desync) is excluded before
/// it reaches this tracker. One failure can still be a one-off, so a single
/// failure raises nothing; two in a row for the same combination with no
/// success between them is already the pattern. In incident 008 the second
/// refusal landed about nine minutes after the first, so a threshold of 2
/// turns a 12-hour human detection into one under ten minutes. Waiting for a
/// third buys little extra confidence and costs another spawn's worth of
/// delay on a low-volume kind (review guides spawn only when a PR changes).
pub const PRE_START_FAILURE_STREAK_THRESHOLD: u32 = 2;

/// Driver label used when no driver slug resolves for an execution.
pub const UNRESOLVED_DRIVER_LABEL: &str = "unknown";

/// The combination a streak is tracked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreakKey {
    pub driver: String,
    pub worker_kind: WorkerKind,
}

impl Ord for StreakKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.driver.as_str(), self.worker_kind.label()).cmp(&(other.driver.as_str(), other.worker_kind.label()))
    }
}
impl PartialOrd for StreakKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl StreakKey {
    pub fn new(driver: impl Into<String>, worker_kind: WorkerKind) -> Self {
        Self {
            driver: driver.into(),
            worker_kind,
        }
    }

    /// Key for an execution of `kind` that resolved to `driver`
    /// ([`UNRESOLVED_DRIVER_LABEL`] when nothing resolved).
    pub fn for_execution(driver: Option<&str>, kind: &ExecutionKind) -> Self {
        let driver = driver
            .map(str::trim)
            .filter(|slug| !slug.is_empty())
            .unwrap_or(UNRESOLVED_DRIVER_LABEL);
        Self::new(driver, worker_kind_for_execution(kind))
    }
}

/// What recording one failure did to its combination's alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureEffect {
    /// Counted, but the streak is still below the threshold: no alert.
    BelowThreshold,
    /// This failure took the streak to the threshold: the alert is new.
    Raised,
    /// An alert was already active and now carries this failure.
    Updated,
}

#[derive(Debug, Clone)]
struct Streak {
    consecutive_failures: u32,
    first_failure_epoch_s: i64,
    latest_failure_epoch_s: i64,
    latest_error: String,
    latest_execution_id: String,
}

/// Per-(driver, worker kind) consecutive pre-start failure counter. One per
/// engine, owned by the coordinator.
///
/// A `std::sync::Mutex` guards the small map and is never held across an
/// `.await`.
///
/// # Why this exists
///
/// Incident 008: for 31 hours every Codex review-guide spawn was refused by
/// the hook-trust gate before a pane existed — 28 consecutive identical
/// failures across 26 work items, zero successes — and no fleet-level
/// alert was raised. Each refusal was handled correctly *per execution* (an ERROR
/// log line, a `spawn_failed` dispatch event, an execution-scoped
/// `pane_spawn_failed` attention item), but nothing looked *across*
/// executions, so a combination that had stopped working entirely looked
/// the same as one unlucky spawn.
///
/// `spawn_health` does aggregate across work items, but only for
/// spawns the app accepted and that then never produced a shell
/// (`spawn_ack_sweep`), and it is not keyed by driver or worker kind. A
/// refusal inside `ExecutionRunner::run_execution` — hook-trust gate, prompt
/// composition, permission config — never reaches it.
///
/// # What it does
///
/// [`PreStartStreakTracker`] counts pre-start spawn outcomes per
/// [`StreakKey`]. At [`PRE_START_FAILURE_STREAK_THRESHOLD`] consecutive
/// failures with no success in between it raises one alert for that
/// combination; every further failure updates that same alert in place
/// (count, latest error, latest execution); the combination's next
/// successful spawn resolves it.
///
/// The alert is **visibility only**. It never pauses dispatch, never makes a
/// failure non-terminal, and never retries anything — the failures keep
/// flowing through their existing per-execution handling untouched. It is
/// also never rate-limited or deduplicated into silence: the count shown is
/// the live count and the error shown is the latest one, in full.
///
/// # Lifetime
///
/// State is in memory and starts empty on every engine boot, the same as
/// `SpawnHealthTracker`. A restart therefore drops an
/// active alert, and a still-broken combination re-raises it after
/// [`PRE_START_FAILURE_STREAK_THRESHOLD`] further failures. That is
/// deliberate: a restart is exactly when the build — and so the verdict —
/// may have changed, and an alert persisted across it would assert a
/// failure the new build has not yet been observed to have.
#[derive(Debug)]
pub struct PreStartStreakTracker {
    streaks: Mutex<BTreeMap<StreakKey, Streak>>,
    /// Generation counter bumped whenever the set of *active alerts* or
    /// their contents change (raise, in-place update, resolve), so the
    /// health broadcaster can push a fresh report without polling.
    alerts_changed: tokio::sync::watch::Sender<u64>,
}

impl Default for PreStartStreakTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PreStartStreakTracker {
    pub fn new() -> Self {
        Self {
            streaks: Mutex::new(BTreeMap::new()),
            alerts_changed: tokio::sync::watch::channel(0).0,
        }
    }

    /// Record one pre-start spawn failure for `key`. Never drops or
    /// coalesces an occurrence: every call increments the count and
    /// replaces the latest error, so an active alert always shows the
    /// current count and the most recent cause.
    pub fn record_failure(
        &self,
        key: StreakKey,
        execution_id: &str,
        error: &str,
        now_epoch_secs: i64,
    ) -> FailureEffect {
        let effect = {
            let mut streaks = self.streaks.lock().expect("pre-start streak lock poisoned");
            let streak = streaks.entry(key).or_insert_with(|| Streak {
                consecutive_failures: 0,
                first_failure_epoch_s: now_epoch_secs,
                latest_failure_epoch_s: now_epoch_secs,
                latest_error: String::new(),
                latest_execution_id: String::new(),
            });
            streak.consecutive_failures = streak.consecutive_failures.saturating_add(1);
            streak.latest_failure_epoch_s = now_epoch_secs;
            streak.latest_error = error.to_owned();
            streak.latest_execution_id = execution_id.to_owned();
            match streak.consecutive_failures {
                n if n < PRE_START_FAILURE_STREAK_THRESHOLD => FailureEffect::BelowThreshold,
                PRE_START_FAILURE_STREAK_THRESHOLD => FailureEffect::Raised,
                _ => FailureEffect::Updated,
            }
        };
        if effect != FailureEffect::BelowThreshold {
            self.notify_alerts_changed();
        }
        effect
    }

    /// Record one successful spawn for `key`, ending its streak. Returns the
    /// alert this resolved, if one was active — `None` when the combination
    /// had no failures or was still below the threshold.
    pub fn record_success(&self, key: &StreakKey) -> Option<SpawnFailureStreak> {
        let removed = self
            .streaks
            .lock()
            .expect("pre-start streak lock poisoned")
            .remove_entry(key);
        let resolved = removed
            .filter(|(_, streak)| streak.consecutive_failures >= PRE_START_FAILURE_STREAK_THRESHOLD)
            .map(|(key, streak)| to_wire(&key, &streak));
        if resolved.is_some() {
            self.notify_alerts_changed();
        }
        resolved
    }

    /// Every combination currently at or past the threshold, ordered by
    /// (driver, worker kind) so the report is stable between reads.
    pub fn active_alerts(&self) -> Vec<SpawnFailureStreak> {
        self.streaks
            .lock()
            .expect("pre-start streak lock poisoned")
            .iter()
            .filter(|(_, streak)| streak.consecutive_failures >= PRE_START_FAILURE_STREAK_THRESHOLD)
            .map(|(key, streak)| to_wire(key, streak))
            .collect()
    }

    /// Subscribe to alert changes. The receiver starts already-seen, so
    /// `changed()` first resolves on the next raise / update / resolve.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.alerts_changed.subscribe()
    }

    fn notify_alerts_changed(&self) {
        self.alerts_changed.send_modify(|generation| {
            *generation = generation.wrapping_add(1);
        });
    }
}

fn to_wire(key: &StreakKey, streak: &Streak) -> SpawnFailureStreak {
    SpawnFailureStreak::builder()
        .driver(key.driver.clone())
        .worker_kind(key.worker_kind.label())
        .consecutive_failures(streak.consecutive_failures)
        .first_failure_epoch_s(streak.first_failure_epoch_s)
        .latest_failure_epoch_s(streak.latest_failure_epoch_s)
        .latest_error(streak.latest_error.clone())
        .latest_execution_id(streak.latest_execution_id.clone())
        .build()
}

#[cfg(test)]
mod tests {
    use crate::{
        PRE_START_FAILURE_STREAK_THRESHOLD, PreStartStreakTracker, StreakKey, UNRESOLVED_DRIVER_LABEL, WorkerKind,
    };
    use boss_protocol::ExecutionKind;

    #[test]
    fn active_alerts_order_worker_kinds_by_label() {
        let tracker = PreStartStreakTracker::default();
        for kind in [WorkerKind::Standard, WorkerKind::ReviewGuide] {
            for _ in 0..PRE_START_FAILURE_STREAK_THRESHOLD {
                tracker.record_failure(StreakKey::new("codex", kind.clone()), "execution", "error", 1);
            }
        }
        let alerts = tracker.active_alerts();
        assert_eq!(
            alerts
                .iter()
                .map(|alert| alert.worker_kind.as_str())
                .collect::<Vec<_>>(),
            vec!["review-guide", "standard"]
        );
    }

    #[test]
    fn streak_key_maps_execution_kind_labels() {
        assert_eq!(
            StreakKey::for_execution(Some("codex"), &ExecutionKind::PrReviewGuide)
                .worker_kind
                .label(),
            "review-guide"
        );
        assert_eq!(
            StreakKey::for_execution(Some("claude"), &ExecutionKind::PrReview)
                .worker_kind
                .label(),
            "reviewer"
        );
        assert_eq!(
            StreakKey::for_execution(None, &ExecutionKind::TaskImplementation).driver,
            UNRESOLVED_DRIVER_LABEL
        );
        assert_eq!(
            StreakKey::for_execution(Some("  "), &ExecutionKind::AutomationTriage)
                .worker_kind
                .label(),
            "triage"
        );
    }
}
