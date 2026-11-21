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
    pub unstarted_claims: std::sync::Mutex<std::collections::HashMap<String, i64>>,
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
        let claims = self.coordinator.progression_worker_pool().claims().await;
        self.unstarted_claims
            .lock()
            .unwrap()
            .retain(|id, _| claims.iter().any(|c| &c.execution_id == id));
        for claim in claims {
            if self.hold_registry.is_held(&claim.execution_id) {
                continue;
            }
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
            let elapsed_secs = now.saturating_sub(started);
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
        } else if execution
            .finished_epoch()
            .is_some_and(|finished| now.saturating_sub(finished) < crate::pool_claim_sweep::LEAK_GRACE_SECS)
        {
            return Ok(false);
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
#[path = "stale_worker_progression_timeout_tests.rs"]
mod tests;
