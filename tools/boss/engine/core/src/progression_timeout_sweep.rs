//! Wall-clock deadlines for progression, independent of worker activity and
//! ordinary stale-worker probes. Iterate pool claims, not live-state entries:
//! spawning, missing registry entries and orphaned rows must not hide a claim.
use std::sync::Arc;
use std::time::Duration;

use boss_execution_contracts::{PaneReleaseOutcome, WorkerPaneReleaser};
use boss_protocol::ExecutionKind;

use crate::coordinator::ExecutionCoordinator;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::pool_claim_sweep::WorkerViewerDetach;
use crate::work::WorkDb;

pub struct ProgressionTimeoutSweep {
    pub work_db: Arc<WorkDb>,
    pub live_states: Arc<LiveWorkerStateRegistry>,
    pub coordinator: Arc<ExecutionCoordinator>,
    pub pane_releaser: Arc<dyn WorkerPaneReleaser>,
    pub viewers: Arc<dyn WorkerViewerDetach>,
}

pub fn spawn_loop(sweep: ProgressionTimeoutSweep) -> tokio::task::JoinHandle<()> {
    let sweep = Arc::new(sweep);
    crate::sweep_loop::spawn_sweep_loop(Duration::from_secs(60), move || {
        let sweep = sweep.clone();
        async move {
            SweepOutcome(
                sweep
                    .run_one_pass(boss_engine_utils::epoch_time::now_epoch_secs())
                    .await,
            )
        }
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
    async fn run_one_pass(&self, now: i64) -> usize {
        let timeout_secs = progression_run_timeout_secs(&self.work_db);
        let mut released = 0;
        for claim in self.coordinator.progression_worker_pool().claims().await {
            let result = self.reconcile(&claim.execution_id, now, timeout_secs).await;
            match result {
                Ok(true) => {
                    // Teardown may already have handed this slot to a new run.
                    self.live_states.release_slot_for_run(&claim.execution_id);
                    self.coordinator
                        .release_pool_claim_if_execution(&claim.worker_id, &claim.execution_id)
                        .await;
                    released += 1;
                }
                Ok(false) => {}
                Err(err) => tracing::warn!(execution_id = %claim.execution_id, %err,
                    "progression deadline cleanup pending; retaining claim for retry"),
            }
        }
        released
    }

    async fn reconcile(&self, execution_id: &str, now: i64, timeout_secs: i64) -> anyhow::Result<bool> {
        let execution = self.work_db.get_execution(execution_id)?;
        anyhow::ensure!(
            execution.kind == ExecutionKind::Progression,
            "non-progression claim in progression pool"
        );
        if !execution.status.is_terminal() {
            // A claimed spawn with no start timestamp is bounded too. Queued
            // executions without a pool claim are never visited here.
            let started = execution.started_epoch().or_else(|| execution.created_epoch());
            let started = started.ok_or_else(|| anyhow::anyhow!("progression run has no valid start time"))?;
            let elapsed_secs = now.saturating_sub(started);
            if elapsed_secs < timeout_secs {
                return Ok(false);
            }
            let reason = format!("progression-run-timeout: elapsed {elapsed_secs}s (limit {timeout_secs}s)");
            // A deadline is a deliberate failure, not inferred death that a
            // late Claude hook may readopt. Preserve an accepted declaration.
            if self.work_db.progression_disposition(execution_id)?.is_some() {
                self.work_db.finish_progression_declared(execution_id)?;
            } else {
                self.work_db.fail_progression_execution(execution_id, &reason)?;
            }
            tracing::warn!(execution_id, elapsed_secs, timeout_secs, "progression deadline reached");
        }

        // Terminal rows still owning claims (including orphaned runs) retry
        // the normal token-verified process and viewer teardown every pass.
        if self.pane_releaser.release_pane(execution_id).await != PaneReleaseOutcome::Reaped {
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
#[path = "stale_worker_progression_timeout_tests.rs"]
mod tests;
