//! Integration of the extracted execution contracts with the coordinator.
use super::helpers::*;
use crate::test_support::{
    AlwaysSucceedsCube, NoopCube, create_ready_chore_execution, create_test_chore, create_test_product,
    insert_host_capability, open_db_arc,
};
use boss_execution_contracts::{ExecutionStartedHook, SendToAppError};

struct LifecycleProbe {
    db: Arc<WorkDb>,
    events: Mutex<Vec<&'static str>>,
    hook_entered: tokio::sync::Notify,
    continue_hook: Semaphore,
    runner_entered: tokio::sync::Notify,
}

#[async_trait]
impl boss_execution_contracts::ExecutionPublisher for LifecycleProbe {
    async fn publish(&self, id: &str, _: &str, _: &str, reason: &str) {
        if reason == "execution_started" {
            assert_eq!(self.db.get_execution(id).unwrap().status, ExecutionStatus::Running);
            self.events.lock().await.push("published");
        }
    }

    async fn publish_work_item_changed(&self, _: &str, _: &str, _: &str) {}
    async fn publish_frontend_event_on_product(&self, _: &str, _: boss_protocol::FrontendEvent) {}
}

#[async_trait]
impl ExecutionStartedHook for LifecycleProbe {
    async fn on_execution_started(&self, id: &str) {
        assert_eq!(self.db.get_execution(id).unwrap().status, ExecutionStatus::Running);
        self.events.lock().await.push("hook");
        self.hook_entered.notify_one();
        self.continue_hook.acquire().await.unwrap().forget();
        self.events.lock().await.push("hook_finished");
    }
}

#[async_trait]
impl boss_execution_contracts::ExecutionRunner for LifecycleProbe {
    async fn run_execution(
        &self,
        _: &str,
        execution: &WorkExecution,
        _: &WorkItem,
        _: &Path,
        _: Option<&str>,
    ) -> Result<RunOutcome> {
        assert_eq!(
            self.db.get_execution(&execution.id).unwrap().status,
            ExecutionStatus::Running
        );
        self.events.lock().await.push("runner");
        self.runner_entered.notify_one();
        Ok(RunOutcome {
            wait_state: RunWaitState::WorkerPaneAlive,
            result_summary: None,
            attention: None,
            slot_id: Some(1),
            spawn_config: None,
        })
    }
}

#[tokio::test]
async fn contracts_preserve_admission_commit_publication_hook_and_spawn_order() {
    let (_dir, db) = open_db_arc();
    insert_host_capability(&db, "local", "driver=claude", "auto");
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "contract lifecycle");
    let execution = create_ready_chore_execution(&db, chore.id);
    let probe = Arc::new(LifecycleProbe {
        db: db.clone(),
        events: Mutex::new(Vec::new()),
        hook_entered: tokio::sync::Notify::new(),
        continue_hook: Semaphore::new(0),
        runner_entered: tokio::sync::Notify::new(),
    });
    let pool = boss_execution_contracts::WorkerPool::new(1);
    let mut coordinator = ExecutionCoordinator::with_publisher(
        db.clone(),
        pool.clone(),
        Arc::new(AlwaysSucceedsCube),
        probe.clone(),
        probe.clone(),
    );
    coordinator.set_execution_started_hook(probe.clone());
    let coordinator = Arc::new(coordinator);
    coordinator.pause_dispatch(
        1,
        DispatchPauseOrigin::Operator,
        PauseReason::new("maintenance").unwrap(),
    );
    assert!(
        coordinator
            .force_dispatch(&execution.id, DispatchAdmission::Queued)
            .await
            .is_err()
    );
    assert!(pool.claims().await.is_empty());
    assert!(probe.events.lock().await.is_empty());
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);

    let dispatch = {
        let coordinator = coordinator.clone();
        let id = execution.id.clone();
        tokio::spawn(async move { coordinator.force_dispatch(&id, DispatchAdmission::OperatorForced).await })
    };
    tokio::time::timeout(Duration::from_secs(5), probe.hook_entered.notified())
        .await
        .unwrap();
    assert_eq!(*probe.events.lock().await, ["published", "hook"]);
    assert_eq!(pool.claims().await[0].execution_id, execution.id);
    probe.continue_hook.add_permits(1);
    assert_eq!(dispatch.await.unwrap().unwrap(), "worker-1");
    tokio::time::timeout(Duration::from_secs(5), probe.runner_entered.notified())
        .await
        .unwrap();
    assert_eq!(
        *probe.events.lock().await,
        ["published", "hook", "hook_finished", "runner"]
    );
    assert_eq!(pool.claims().await[0].execution_id, execution.id);
}

struct FailedSendRunner(SendToAppError);

#[async_trait]
impl boss_execution_contracts::ExecutionRunner for FailedSendRunner {
    async fn run_execution(
        &self,
        _: &str,
        _: &WorkExecution,
        _: &WorkItem,
        _: &Path,
        _: Option<&str>,
    ) -> Result<RunOutcome> {
        Err(anyhow::Error::new(self.0.clone()).context("worker request failed"))
    }
}

#[tokio::test]
async fn host_provider_and_runner_preserve_typed_app_send_errors() {
    let (_dir, db) = open_db_arc();
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id, "contract errors");
    let execution = create_ready_chore_execution(&db, chore.id.clone());
    let item = db.get_work_item(&chore.id).unwrap();
    let host = db.get_host("local").unwrap().unwrap();
    for send_error in [
        SendToAppError::NotRegistered,
        SendToAppError::SessionWedged,
        SendToAppError::AppDisconnected,
        SendToAppError::Timeout,
        SendToAppError::ResponseKindMismatch("AttachWorkerPane"),
    ] {
        let expected = send_error.to_string();
        let local = Arc::new(LocalHostAdapter::new(
            Arc::new(NoopCube),
            Arc::new(FailedSendRunner(send_error)),
        ));
        let provider: Arc<dyn boss_execution_contracts::HostAdapterProvider> =
            Arc::new(LocalHostAdapterProvider::new(local));
        let adapter = provider.adapter_for(&host).await.unwrap();
        let err = adapter
            .spawn_worker("worker-1", &execution, &item, Path::new("/unused"), None)
            .await
            .unwrap_err();
        // The compatibility path and the lower contract are the same error type.
        assert_eq!(
            err.downcast_ref::<crate::app::SendToAppError>().unwrap().to_string(),
            expected
        );
        assert_eq!(err.to_string(), "worker request failed");
    }
}
