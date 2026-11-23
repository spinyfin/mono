//! Awake-time deadlines for progression, independent of worker activity and
//! ordinary stale-worker probes. Iterate pool claims, not live-state entries:
//! spawning, missing registry entries and orphaned rows must not hide a claim.
//!
//! Elapsed time is host-awake time: a monotonic clock that does not advance
//! during suspend, compared against wall-clock so a lid-close never consumes
//! the budget. A run that genuinely exceeds the awake-time limit still fails.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use boss_execution_contracts::{PaneReleaseOutcome, WorkerPaneReleaser};
use boss_protocol::ExecutionKind;

use crate::coordinator::ExecutionCoordinator;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::pool_claim_sweep::{TeardownRetries, WorkerViewerDetach};
use crate::work::WorkDb;

/// Wall gap larger than this on first observation is treated as suspend (or
/// an engine restart after sleep), not as run time. Larger than the five-minute
/// fixture age used by timeout tests, smaller than a closed-lid sleep.
const SUSPEND_GAP_SECS: i64 = 10 * 60;

/// Wall-clock and awake-time sample for one sweep pass.
#[derive(Debug, Clone, Copy)]
struct DeadlineClock {
    wall_epoch_secs: i64,
    /// Seconds from a clock that does not advance during suspend.
    awake_secs: i64,
}

impl DeadlineClock {
    #[cfg(test)]
    fn wall(now: i64) -> Self {
        Self {
            wall_epoch_secs: now,
            awake_secs: now,
        }
    }

    fn live() -> Self {
        Self {
            wall_epoch_secs: boss_engine_utils::epoch_time::now_epoch_secs(),
            awake_secs: process_awake_secs(),
        }
    }
}

fn process_awake_secs() -> i64 {
    static START: OnceLock<Instant> = OnceLock::new();
    i64::try_from(START.get_or_init(Instant::now).elapsed().as_secs()).unwrap_or(i64::MAX)
}

struct AwakeWatch {
    last_wall: i64,
    last_awake: i64,
    elapsed: i64,
}

#[derive(bon::Builder)]
#[builder(on(String, into))]
pub struct ProgressionTimeoutSweep {
    pub work_db: Arc<WorkDb>,
    pub live_states: Arc<LiveWorkerStateRegistry>,
    pub coordinator: Arc<ExecutionCoordinator>,
    pub pane_releaser: Arc<dyn WorkerPaneReleaser>,
    pub hold_registry: Arc<crate::hold_registry::HoldRegistry>,
    pub dispatch_events: Arc<dyn crate::dispatch_events::DispatchEventSink>,
    #[builder(default)]
    unstarted_claims: Mutex<HashMap<String, i64>>,
    #[builder(skip)]
    awake_watches: Mutex<HashMap<String, AwakeWatch>>,
    #[builder(default)]
    teardown_retries: Mutex<TeardownRetries>,
    pub viewers: Arc<dyn WorkerViewerDetach>,
}

pub fn spawn_loop(sweep: ProgressionTimeoutSweep) -> tokio::task::JoinHandle<()> {
    let sweep = Arc::new(sweep);
    crate::sweep_loop::spawn_sweep_loop(Duration::from_secs(60), move || {
        let sweep = sweep.clone();
        async move { SweepOutcome(sweep.run_deadline_pass(DeadlineClock::live()).await) }
    })
}

struct SweepOutcome(usize);

impl crate::sweep_loop::SweepOutcome for SweepOutcome {
    fn has_activity(&self) -> bool {
        self.0 > 0
    }

    fn log(&self) {
        tracing::info!(released = self.0, "progression deadline sweep: retired claims");
    }
}

fn progression_run_timeout_secs(work_db: &WorkDb) -> i64 {
    use boss_engine_storage_values::progression::{DEFAULT_RUN_TIMEOUT_SECS, METADATA_KEY_RUN_TIMEOUT_SECS};
    work_db
        .get_metadata(METADATA_KEY_RUN_TIMEOUT_SECS)
        .ok()
        .flatten()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(i64::try_from(DEFAULT_RUN_TIMEOUT_SECS).unwrap_or(i64::MAX))
}

impl ProgressionTimeoutSweep {
    #[cfg(test)]
    async fn run_one_pass(&self, now: i64) -> usize {
        self.run_deadline_pass(DeadlineClock::wall(now)).await
    }

    async fn run_deadline_pass(&self, clock: DeadlineClock) -> usize {
        let timeout_secs = progression_run_timeout_secs(&self.work_db);
        let mut released = 0;
        let claims = self.coordinator.progression_worker_pool().claims().await;
        self.unstarted_claims
            .lock()
            .unwrap()
            .retain(|id, _| claims.iter().any(|c| &c.execution_id == id));
        self.awake_watches
            .lock()
            .unwrap()
            .retain(|id, _| claims.iter().any(|c| &c.execution_id == id));
        let claimed = claims.iter().map(|claim| claim.execution_id.clone()).collect();
        self.teardown_retries.lock().unwrap().retain_claimed(&claimed);
        for claim in claims {
            if self.hold_registry.is_held(&claim.execution_id) {
                continue;
            }
            let result = self.reconcile(&claim.execution_id, clock, timeout_secs).await;
            match result {
                Ok(true) => {
                    self.teardown_retries.lock().unwrap().succeeded(&claim.execution_id);
                    // Teardown may already have handed this slot to a new run.
                    self.live_states.release_slot_for_run(&claim.execution_id);
                    self.coordinator
                        .release_pool_claim_if_execution(&claim.worker_id, &claim.execution_id)
                        .await;
                    released += 1;
                }
                Ok(false) => {}
                Err(err) => {
                    // Escalates to the same attention item as the pool-claim
                    // sweep after repeated unconfirmed teardowns.
                    self.teardown_retries.lock().unwrap().failed(
                        &self.work_db,
                        &claim.execution_id,
                        &format!("{err:#}"),
                    );
                }
            }
        }
        released
    }

    fn awake_elapsed(&self, execution_id: &str, started: i64, clock: DeadlineClock) -> i64 {
        let mut watches = self.awake_watches.lock().unwrap();
        if let Some(watch) = watches.get_mut(execution_id) {
            let wall_gap = clock.wall_epoch_secs.saturating_sub(watch.last_wall);
            let awake_gap = clock.awake_secs.saturating_sub(watch.last_awake);
            watch.elapsed = watch.elapsed.saturating_add(wall_gap.min(awake_gap));
            watch.last_wall = clock.wall_epoch_secs;
            watch.last_awake = clock.awake_secs;
            watch.elapsed
        } else {
            let from_start = clock.wall_epoch_secs.saturating_sub(started);
            let elapsed = if from_start > SUSPEND_GAP_SECS { 0 } else { from_start };
            watches.insert(
                execution_id.to_owned(),
                AwakeWatch {
                    last_wall: clock.wall_epoch_secs,
                    last_awake: clock.awake_secs,
                    elapsed,
                },
            );
            elapsed
        }
    }

    async fn reconcile(&self, execution_id: &str, clock: DeadlineClock, timeout_secs: i64) -> anyhow::Result<bool> {
        let now = clock.wall_epoch_secs;
        let execution = self.work_db.get_execution(execution_id)?;
        anyhow::ensure!(
            execution.kind == ExecutionKind::Progression,
            "non-progression claim in progression pool"
        );
        if !execution.status.is_terminal() {
            // Unstarted claims get a full timeout from first observation.
            // Adopted running panes retain the durable creation-time fallback.
            let started = execution.started_epoch().or_else(|| {
                if matches!(
                    execution.status,
                    boss_protocol::ExecutionStatus::Ready | boss_protocol::ExecutionStatus::Claimed
                ) {
                    Some(
                        *self
                            .unstarted_claims
                            .lock()
                            .unwrap()
                            .entry(execution_id.to_owned())
                            .or_insert(now),
                    )
                } else {
                    execution.created_epoch()
                }
            });
            let started = started.ok_or_else(|| anyhow::anyhow!("progression run has no valid start time"))?;
            let elapsed_secs = self.awake_elapsed(execution_id, started, clock);
            if elapsed_secs < timeout_secs {
                return Ok(false);
            }
            let reason = format!("progression-run-timeout: elapsed {elapsed_secs}s (limit {timeout_secs}s)");
            // A deadline is a deliberate failure, not inferred death that a
            // late Claude hook may readopt. Preserve an accepted declaration.
            let finalized = self.work_db.finish_progression_at_deadline(execution_id, &reason)?;
            let Some(finalized) = finalized else {
                // Another finalizer owns teardown and its terminal grace.
                return Ok(false);
            };
            {
                use crate::dispatch_events::{DispatchEvent, Outcome, Stage};
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Ok, execution_id)
                            .with_work_item(&execution.work_item_id)
                            .with_details(serde_json::json!({
                                "reason": "progression_run_timeout", "elapsed_secs": elapsed_secs,
                                "timeout_secs": timeout_secs, "execution_status": finalized.status,
                            })),
                    )
                    .await;
            }
            tracing::warn!(execution_id, elapsed_secs, timeout_secs, "progression deadline reached");
        } else {
            // An inferred-orphaned run that a live worker still backs may be
            // re-adopted by a late hook (see the worker liveness contract), so
            // it keeps the full run timeout instead of the short terminal grace.
            let live_backed = self
                .live_states
                .snapshot()
                .iter()
                .any(|state| state.run_id == execution_id);
            if live_backed && execution.status == boss_protocol::ExecutionStatus::Orphaned {
                let started = execution
                    .started_epoch()
                    .or_else(|| execution.created_epoch())
                    .ok_or_else(|| anyhow::anyhow!("progression run has no valid start time"))?;
                if self.awake_elapsed(execution_id, started, clock) < timeout_secs {
                    return Ok(false);
                }
            }
            if execution
                .finished_epoch()
                .is_some_and(|finished| now.saturating_sub(finished) < crate::pool_claim_sweep::LEAK_GRACE_SECS)
            {
                return Ok(false);
            }
        }

        // Terminal rows still owning claims (including orphaned runs) retry
        // the normal token-verified process and viewer teardown every pass.
        if self.work_db.tmux_identity_for_execution(execution_id)?.is_some()
            && self.pane_releaser.release_pane(execution_id).await != PaneReleaseOutcome::Reaped
        {
            anyhow::bail!("process teardown unconfirmed");
        }
        let result = self.viewers.confirm_viewers_detached(&[execution_id.to_owned()]).await;
        match result.into_iter().next() {
            Some(Ok(())) => {}
            Some(Err(reason)) => anyhow::bail!(reason),
            None => anyhow::bail!("viewer detach result missing"),
        }
        crate::driver_teardown::teardown_driver_workspace(
            &self.work_db,
            execution_id,
            execution.workspace_path.as_deref().map(std::path::Path::new),
            crate::driver_teardown::TeardownReason::StaleWorkerReconcile,
        )
        .await;
        Ok(true)
    }
}

#[cfg(test)]
#[path = "progression_timeout_sweep_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "progression_timeout_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "progression_deadline_regression_tests.rs"]
mod regression_tests;
