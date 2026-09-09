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
    // A live tmux session is tmux-hosted by construction. Do not read
    // `latest_run_tmux_hosting_for_execution` here: that query takes the
    // newest run row, and a later local bookkeeping sibling (the shape
    // `non_terminal_match_with_newer_sibling_run_still_rebuilds_derived_state`
    // reproduces) would fold this worker into the legacy terminate-on-quit
    // claim — the same wrong-in-one-direction lie this stamp exists to stop.
    live_states.register_readoption(
        slot_id,
        execution_id.to_owned(),
        model_label,
        shell_pid,
        binding,
        awaiting_input_capable,
        LiveSpawnRouting::new_with_hosting(Some(pool.to_owned()), execution.kind.as_str(), true),
        ReadoptionEvidence::LiveShellPid,
    );
    match work_db.get_run_semantic_progress_checkpoint(execution_id) {
        Ok(Some(checkpoint)) => live_states.seed_semantic_progress(slot_id, &checkpoint),
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "tmux boot adoption: could not load the semantic-progress checkpoint; this worker will \
                 be reaped as a never-started driver if no checkpoint or hook arrives within the grace \
                 window",
            );
        }
    }
    spawner.publish_live_worker_states().await;
    if let Some(driver) = driver {
        spawner.start_live_status_slot(slot_id, execution_id, driver);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::SendToAppError;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::protocol::{EngineToAppRequest, EngineToAppResponse};
    use crate::test_support::{create_test_chore_manual, create_test_product, open_db};
    use crate::worker_registry::WorkerRegistry;
    use boss_protocol::RequestExecutionInput;
    use tokio::time::Duration;

    #[derive(Default)]
    struct LiveStateSpawner {
        registry: WorkerRegistry,
        live_states: LiveWorkerStateRegistry,
    }

    #[async_trait::async_trait]
    impl WorkerSpawner for LiveStateSpawner {
        async fn send_to_app_request(
            &self,
            _request: EngineToAppRequest,
            _timeout: Duration,
        ) -> Result<EngineToAppResponse, SendToAppError> {
            panic!("register_adopted_live_state must not call the app RPC");
        }

        fn worker_registry(&self) -> &WorkerRegistry {
            &self.registry
        }

        fn live_worker_state_registry(&self) -> Option<&LiveWorkerStateRegistry> {
            Some(&self.live_states)
        }
    }

    fn seed_running(db: &WorkDb, tmux_hosted: bool) -> WorkExecution {
        let product = create_test_product(db);
        let chore = create_test_chore_manual(db, product.id.clone(), "adopted");
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
            .unwrap();
        let (execution, _run) = db
            .start_execution_run_on_host_with_tmux_hosting(
                &execution.id,
                "worker-1",
                "mono",
                "lease-1",
                "ws-1",
                "/tmp/ws-1",
                "local",
                tmux_hosted,
            )
            .unwrap();
        execution
    }

    #[tokio::test]
    async fn adopted_tmux_hosted_run_stamps_live_state_tmux_hosted() {
        let (_dir, db) = open_db();
        let execution = seed_running(&db, true);
        let spawner = LiveStateSpawner::default();
        register_adopted_live_state(&db, &spawner, &execution, &execution.id, 1, 4321, None).await;
        let state = spawner.live_states.get(1).expect("slot registered");
        assert_eq!(state.tmux_hosted, Some(true));
        assert_eq!(state.run_id, execution.id);
    }

    #[tokio::test]
    async fn adopted_live_tmux_session_stamps_true_even_if_newest_run_row_says_legacy() {
        let (_dir, db) = open_db();
        let execution = seed_running(&db, false);
        let spawner = LiveStateSpawner::default();
        register_adopted_live_state(&db, &spawner, &execution, &execution.id, 2, 4321, None).await;
        let state = spawner.live_states.get(2).expect("slot registered");
        assert_eq!(
            state.tmux_hosted,
            Some(true),
            "a live tmux session survives quit; a shadowed or false newest-row bit must not reclassify it as legacy"
        );
    }
}
