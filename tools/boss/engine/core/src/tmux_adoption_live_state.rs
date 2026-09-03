//! Live-state reconstruction for a tmux-adopted worker.
//!
//! Split out of [`crate::tmux_adoption`] so that module stays under the
//! file-size budget while still seeding re-adopted slots from the durable
//! semantic-progress checkpoint.

use std::sync::Arc;

use crate::driver::AgentDriver;
use crate::live_worker_state::{LiveSpawnRouting, ReadoptionEvidence, attributed_pool_label};
use crate::spawn_flow::WorkerSpawner;
use crate::work::{WorkDb, WorkExecution};

/// Rebuild the live-state entry, live-status summarizer, and semantic
/// progress for one adopted run.
pub(crate) async fn register_adopted_live_state<S>(
    work_db: &WorkDb,
    spawner: &S,
    execution: &WorkExecution,
    execution_id: &str,
    slot_id: u8,
    shell_pid: i32,
    driver: Option<Arc<dyn AgentDriver>>,
) where
    S: WorkerSpawner + ?Sized,
{
    let Some(live_states) = spawner.live_worker_state_registry() else {
        return;
    };
    let binding = work_db
        .get_work_item(&execution.work_item_id)
        .ok()
        .map(|item| boss_protocol::WorkItemBinding {
            work_item_id: execution.work_item_id.clone(),
            work_item_name: crate::runner::work_item_name(&item).to_owned(),
            execution_id: execution_id.to_owned(),
        });
    let has_source_automation = matches!(
        work_db.source_automation_id_for_work_item(&execution.work_item_id),
        Ok(Some(_))
    );
    let pool = attributed_pool_label(execution.kind.clone(), has_source_automation);
    let model_label = driver
        .as_ref()
        .map(|driver| driver.descriptor().label.to_owned())
        .unwrap_or_else(|| crate::effort::ENGINE_DEFAULT_DRIVER.to_owned());
    let awaiting_input_capable = driver.as_ref().is_some_and(|driver| {
        driver
            .capabilities()
            .provides(crate::driver::Capability::AwaitingInputSignal)
    });
    live_states.register_readoption(
        slot_id,
        execution_id.to_owned(),
        model_label,
        shell_pid,
        binding,
        awaiting_input_capable,
        LiveSpawnRouting::new(pool, execution.kind.as_str()),
        ReadoptionEvidence::LiveShellPid,
    );
    match work_db.get_run_semantic_progress_checkpoint(execution_id) {
        Ok(Some(checkpoint)) => live_states.seed_semantic_progress(slot_id, &checkpoint),
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "tmux boot adoption: could not load the semantic-progress checkpoint; \
                 leaving re-adopted live state unknown until a driver event arrives",
            );
        }
    }
    spawner.publish_live_worker_states().await;
    if let Some(driver) = driver {
        spawner.start_live_status_slot(slot_id, execution_id, driver);
    }
}
