//! An explicit start that arrives while the engine is still starting a run
//! reports that run; it never replaces it.
//!
//! The live-worker registry learns of a worker only once its pane's shell
//! exists. Between pickup and that point the row is `claimed`, then `running`
//! with its run committed, and the registry has nothing to say about it. A
//! failed spawn has the mirror-image window while its workspace is torn down.
//! An explicit start that trusted the registry alone treated all of these as
//! "worker gone", abandoned the run that had just been dispatched, and left a
//! successor waiting on the abandoned run's workspace lease.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use crate::live_worker_state::LiveWorkerStateRegistry;

/// What `RequestExecution { force: true }` (`bossctl agents launch`) does once
/// it has named a work item: create or reuse the execution, force-dispatch it
/// only when that left a `ready` row, and answer with the row as it now is.
async fn agents_launch(
    coordinator: &Arc<ExecutionCoordinator>,
    live: &Arc<LiveWorkerStateRegistry>,
    work_item_id: &str,
) -> Result<WorkExecution> {
    let execution = coordinator.request_execution_via_db(
        RequestExecutionInput::builder()
            .work_item_id(work_item_id)
            .explicit_start(true)
            .force(true)
            .build(),
        live.clone(),
    )?;
    if execution.status == ExecutionStatus::Ready {
        coordinator
            .force_dispatch(&execution.id, DispatchAdmission::OperatorForced)
            .await?;
    }
    coordinator.work_db().get_execution(&execution.id)
}

struct Dispatched {
    _dir: tempfile::TempDir,
    cube: Arc<FakeCubeClient>,
    coordinator: Arc<ExecutionCoordinator>,
    live: Arc<LiveWorkerStateRegistry>,
    /// The work item's first execution, as queued before any dispatch.
    queued: WorkExecution,
}

impl Dispatched {
    /// An autostart chore with its first execution queued, and a coordinator
    /// whose live-worker registry is empty: no pane is ever registered, as in
    /// the window before a spawn produces one.
    fn new(cube: FakeCubeClient, runner: FakeExecutionRunner) -> Self {
        let dir = tempdir().unwrap();
        let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, product.id.clone(), "Just dispatched");
        db.reconcile_product_executions(&product.id).unwrap();
        let queued = db.list_executions(Some(&chore.id)).unwrap().remove(0);
        let cube = Arc::new(cube);
        let live = Arc::new(LiveWorkerStateRegistry::new());
        let mut coordinator = ExecutionCoordinator::new(db, WorkerPool::new(2), cube.clone(), Arc::new(runner));
        coordinator.set_live_worker_states(live.clone());
        Self {
            _dir: dir,
            cube,
            coordinator: Arc::new(coordinator),
            live,
            queued,
        }
    }

    fn db(&self) -> &WorkDb {
        self.coordinator.work_db()
    }

    /// The dispatched run as the database has it now.
    fn run(&self) -> WorkExecution {
        self.db().get_execution(&self.queued.id).unwrap()
    }

    /// A plain explicit start: `bossctl work start`, or a kanban drag.
    fn work_start(&self) -> WorkExecution {
        self.coordinator
            .request_execution_via_db(
                RequestExecutionInput::builder()
                    .work_item_id(self.queued.work_item_id.clone())
                    .explicit_start(true)
                    .build(),
                self.live.clone(),
            )
            .unwrap()
    }

    fn executions(&self) -> Vec<WorkExecution> {
        self.db().list_executions(Some(&self.queued.work_item_id)).unwrap()
    }

    /// The launch answered with the run the dispatcher started, and that run
    /// is the only one the work item has.
    async fn assert_launch_reports(&self, expected: ExecutionStatus) {
        assert!(
            !self.live.is_run_live(&self.queued.id),
            "the fixture models a run no pane has been registered for"
        );
        let launched = agents_launch(&self.coordinator, &self.live, &self.queued.work_item_id)
            .await
            .expect("a redundant launch reports the existing run");
        assert_eq!(launched.id, self.queued.id, "the launch must report the dispatched run");
        assert_eq!(launched.status, expected);
        let executions = self.executions();
        assert_eq!(
            executions.len(),
            1,
            "the launch must not create a second execution: {executions:?}"
        );
        assert_eq!(
            self.cube.lease_calls.lock().await.len(),
            1,
            "the launch must not lease a second workspace"
        );
    }
}

/// The reported incident: the dispatcher starts a run, and a launch for the
/// same item arrives while that run's pane is still being created.
#[tokio::test]
async fn a_launch_right_behind_a_dispatch_reports_the_run_being_started_and_leaves_it_running() {
    let s = Dispatched::new(
        FakeCubeClient::default(),
        // The spawn never returns: the run has started, its pane has not.
        FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        },
    );
    s.coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(s.db(), &s.queued.id, ExecutionStatus::Running).await;

    s.assert_launch_reports(ExecutionStatus::Running).await;
    assert!(s.run().cube_lease_id.is_some(), "the run keeps the workspace it leased");

    // A plain explicit start (`bossctl work start`, a kanban drag) lands the same way.
    let started = s.work_start();
    assert_eq!(started.id, s.queued.id);
    assert_eq!(started.status, ExecutionStatus::Running);
    assert_eq!(s.executions().len(), 1);
}

/// The same request one step earlier: the row is `claimed` and its workspace
/// lease has not come back yet.
#[tokio::test]
async fn a_launch_while_the_dispatch_is_still_leasing_reports_the_claimed_run() {
    let s = Dispatched::new(
        FakeCubeClient {
            hang_lease: true,
            ..FakeCubeClient::default()
        },
        FakeExecutionRunner::default(),
    );
    s.coordinator.drain_ready_queue_for_test().await;
    for _ in 0..3000 {
        if !s.cube.lease_calls.lock().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(s.run().status, ExecutionStatus::Claimed);

    s.assert_launch_reports(ExecutionStatus::Claimed).await;
}

/// A spawn that failed is still the engine's to finish: its row stays
/// `running` while the workspace is preserved and released, which can take
/// minutes. A launch in that window must not abandon the row out from under
/// the teardown, or the failure is never recorded and the launch's own
/// execution is left waiting on the lease being released.
#[tokio::test]
async fn a_launch_while_a_failed_spawn_is_being_torn_down_does_not_abandon_the_run() {
    let dir = tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let s = Dispatched::new(
        FakeCubeClient {
            release_gate: Some(gate.clone()),
            ..FakeCubeClient::default()
        }
        .with_real_workspace(dir.path()),
        FakeExecutionRunner {
            fail: true,
            ..FakeExecutionRunner::default()
        },
    );
    s.coordinator.drain_ready_queue_for_test().await;
    // The fake records the release before parking on its gate, so this is the
    // teardown held open mid-release.
    for _ in 0..3000 {
        if !s.cube.release_calls.lock().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !s.cube.release_calls.lock().await.is_empty(),
        "the failed spawn's workspace release was never entered"
    );
    assert_eq!(
        s.run().status,
        ExecutionStatus::Running,
        "the row must still be mid-teardown for the launch to land in the window"
    );

    s.assert_launch_reports(ExecutionStatus::Running).await;

    gate.notify_one();
    wait_for_execution_status(s.db(), &s.queued.id, ExecutionStatus::Failed).await;
}

/// The other direction stays as it was: a `running` row that nothing is
/// starting and no pane backs is stale, and an explicit start replaces it. An
/// engine restart is what leaves such a row, so the test drops the in-flight
/// spawn the way a restart does.
#[tokio::test]
async fn an_explicit_start_still_replaces_a_run_that_nothing_is_starting() {
    let s = Dispatched::new(
        FakeCubeClient::default(),
        FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        },
    );
    s.coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(s.db(), &s.queued.id, ExecutionStatus::Running).await;
    s.coordinator.abort_driving_runs();

    let replacement = s.work_start();
    assert_ne!(replacement.id, s.queued.id);
    assert_eq!(replacement.status, ExecutionStatus::Ready);
    assert_eq!(s.run().status, ExecutionStatus::Abandoned);
}
