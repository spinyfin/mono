//! The checkout-free spawn of a `progression` run: it takes no cube lease and
//! makes no cube call, runs in an engine-owned scratch directory, and leaves
//! the stuck work item exactly as it found it — including when the item is
//! gated by an unmet prerequisite and when the spawn itself fails.

use super::helpers::*;

struct Fixture {
    db: Arc<WorkDb>,
    product: String,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db).id;
    Fixture { db, product, _dir: dir }
}

/// The task columns the ordinary run start rewrites (status, autostart and the
/// `dispatch_failed_*` stamp), plus the fields a reconcile audit would touch.
fn item_snapshot(db: &WorkDb, id: &str) -> Vec<Option<String>> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status, CAST(autostart AS TEXT), dispatch_failed_reason, dispatch_failed_error,
                    dispatch_failed_at, updated_at, description
             FROM tasks WHERE id = ?1",
            [id],
            |row| (0..7).map(|i| row.get::<_, Option<String>>(i)).collect(),
        )
        .unwrap()
}

impl Fixture {
    /// A stuck item carrying the evidence a run start would erase, with a
    /// `ready` progression execution bound to it.
    fn stuck_item_with_progression_run(&self) -> (String, WorkExecution) {
        let chore = create_test_chore_manual(&self.db, self.product.clone(), "Stuck item");
        self.db
            .connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'todo', autostart = 1, dispatch_failed_reason = 'cube_workspace_lease_failed',
                        dispatch_failed_error = 'lease boom', dispatch_failed_at = '2026-11-01T00:00:00Z'
                 WHERE id = ?1",
                [&chore.id],
            )
            .unwrap();
        let run = self
            .db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::Progression)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        (chore.id, run)
    }

    fn coordinator(
        &self,
        runner: FakeExecutionRunner,
    ) -> (Arc<ExecutionCoordinator>, Arc<FakeCubeClient>, Arc<FakeExecutionRunner>) {
        let cube = Arc::new(FakeCubeClient::default());
        let runner = Arc::new(runner);
        let mut coordinator =
            ExecutionCoordinator::new(self.db.clone(), WorkerPool::new(16), cube.clone(), runner.clone());
        coordinator.set_progression_pool(WorkerPool::new_progression(1));
        coordinator.set_progression_spawn_enabled(true);
        (Arc::new(coordinator), cube, runner)
    }
}

async fn assert_no_cube_call(cube: &FakeCubeClient) {
    assert!(cube.ensure_calls.lock().await.is_empty(), "no `repo ensure`");
    assert!(cube.lease_calls.lock().await.is_empty(), "no lease");
    assert!(cube.goto_calls.lock().await.is_empty(), "no goto");
    assert!(cube.create_calls.lock().await.is_empty(), "no change create");
    assert!(cube.release_calls.lock().await.is_empty(), "no release");
    assert!(cube.heartbeat_calls.lock().await.is_empty(), "no lease heartbeat");
}

#[tokio::test]
async fn live_progression_does_not_replace_or_abandon_requested_implementation_work() {
    let f = fixture();
    let (item_id, run) = f.stuck_item_with_progression_run();
    let (coordinator, _cube, runner) = f.coordinator(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;
    let progression_before = f.db.get_execution(&run.id).unwrap();
    assert!(f.db.live_writers_in_chain(&item_id).unwrap().is_empty());

    let implementation =
        f.db.request_execution_with_live_check(
            RequestExecutionInput::builder()
                .work_item_id(&item_id)
                .priority(99)
                .build(),
            |id| id == run.id,
        )
        .unwrap();
    assert_ne!(implementation.id, run.id);
    assert_eq!(implementation.kind, ExecutionKind::ChoreImplementation);
    assert_eq!(implementation.status, ExecutionStatus::Ready);
    let progression_after = f.db.get_execution(&run.id).unwrap();
    assert_eq!(progression_after.priority, progression_before.priority);
    assert_eq!(progression_after.status, ExecutionStatus::Running);

    coordinator
        .force_dispatch(&implementation.id, DispatchAdmission::OperatorForced)
        .await
        .unwrap();
    wait_for_execution_status(&f.db, &implementation.id, ExecutionStatus::Running).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while runner.calls.lock().await.len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both workers reach the execution runner");
    assert_eq!(runner.calls.lock().await.len(), 2);
    let writers = f.db.live_writers_in_chain(&item_id).unwrap();
    assert_eq!(writers.len(), 1);
    assert_eq!(writers[0].id, implementation.id);
    // Progression scheduling must still see real live work on its item.
    assert_eq!(
        f.db.get_live_execution_for_work_item(&item_id, &run.id)
            .unwrap()
            .unwrap()
            .id,
        implementation.id
    );
    let _ = boss_execution_contracts::progression_scratch::remove_scratch_dir(&run.id);
}

#[test]
fn progression_dedup_still_sees_another_live_progression_run() {
    let f = fixture();
    let (item_id, run) = f.stuck_item_with_progression_run();
    let live =
        f.db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&item_id)
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();
    assert_eq!(
        f.db.get_live_execution_for_work_item(&item_id, &run.id)
            .unwrap()
            .unwrap()
            .id,
        live.id
    );
    assert!(f.db.get_live_execution_for_work_item(&item_id, "").unwrap().is_none());
}

#[tokio::test]
async fn a_progression_run_takes_no_lease_runs_in_a_scratch_dir_and_leaves_the_item_untouched() {
    let f = fixture();
    let (item_id, run) = f.stuck_item_with_progression_run();
    let before = item_snapshot(&f.db, &item_id);
    let (coordinator, cube, runner) = f.coordinator(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;

    assert_no_cube_call(&cube).await;
    let started = f.db.get_execution(&run.id).unwrap();
    assert_eq!(started.cube_lease_id, None);
    assert_eq!(started.cube_workspace_id, None);
    let scratch = boss_execution_contracts::progression_scratch::scratch_dir_for(&run.id).unwrap();
    assert_eq!(started.workspace_path.as_deref(), Some(scratch.to_str().unwrap()));
    assert!(scratch.is_dir(), "the scratch directory exists while the run does");
    assert!(
        f.db.latest_run_host_for_execution(&run.id).unwrap().as_deref() == Some("local"),
        "the run is recorded on the local host"
    );

    let calls = runner.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].2,
        scratch.display().to_string(),
        "the agent's cwd is the scratch dir"
    );
    assert_eq!(calls[0].3, None, "no cube change");
    drop(calls);

    assert_eq!(
        item_snapshot(&f.db, &item_id),
        before,
        "starting the run must not advance, clear or stamp the stuck item"
    );
    let _ = boss_execution_contracts::progression_scratch::remove_scratch_dir(&run.id);
}

#[tokio::test]
async fn the_attention_reconcile_backstop_does_not_read_a_progression_run_as_the_item_resuming() {
    let f = fixture();
    let (item_id, run) = f.stuck_item_with_progression_run();
    f.db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_attention_items
                 (id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at)
             VALUES ('attn_stuck_evidence', NULL, ?1, ?2, 'open', 'stuck', 'stuck', '1000', NULL)",
            rusqlite::params![item_id, crate::work::CHURN_GUARD_PARKED_ATTENTION_KIND],
        )
        .unwrap();
    let before = item_snapshot(&f.db, &item_id);
    let (coordinator, _cube, _runner) = f.coordinator(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;
    f.db.reconcile_stale_attention_signals().unwrap();

    assert_eq!(
        item_snapshot(&f.db, &item_id),
        before,
        "the backstop must keep the dispatch_failed_* evidence while only a progression run has started"
    );
    let status: String =
        f.db.connect()
            .unwrap()
            .query_row(
                "SELECT status FROM work_attention_items WHERE id = 'attn_stuck_evidence'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(
        status, "open",
        "a WorkResumed attention must survive a progression run start"
    );
    let _ = boss_execution_contracts::progression_scratch::remove_scratch_dir(&run.id);
}

#[tokio::test]
async fn a_progression_run_on_a_gated_item_still_spawns() {
    let f = fixture();
    let (item_id, run) = f.stuck_item_with_progression_run();
    let prereq = create_test_chore_manual(&f.db, f.product.clone(), "Unmet prerequisite");
    f.db.add_dependency(AddDependencyInput {
        dependent: item_id.clone(),
        prerequisite: prereq.id,
        relation: None,
    })
    .unwrap();
    assert!(!f.db.gating_prereqs_for(&item_id).unwrap().is_empty());
    let (coordinator, cube, _runner) = f.coordinator(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Running).await;

    assert_no_cube_call(&cube).await;
    let _ = boss_execution_contracts::progression_scratch::remove_scratch_dir(&run.id);
}

#[tokio::test]
async fn a_failed_progression_spawn_fails_the_run_but_not_the_item_and_removes_the_scratch_dir() {
    let f = fixture();
    let (item_id, run) = f.stuck_item_with_progression_run();
    let before = item_snapshot(&f.db, &item_id);
    let (coordinator, cube, _runner) = f.coordinator(FakeExecutionRunner {
        fail: true,
        ..FakeExecutionRunner::default()
    });

    coordinator.drain_ready_queue_for_test().await;
    wait_for_execution_status(&f.db, &run.id, ExecutionStatus::Failed).await;

    assert_no_cube_call(&cube).await;
    assert_eq!(
        item_snapshot(&f.db, &item_id),
        before,
        "a failed run never demotes the item"
    );
    let scratch = boss_execution_contracts::progression_scratch::scratch_dir_for(&run.id).unwrap();
    assert!(!scratch.exists(), "teardown removes the scratch directory");
    assert!(f.db.list_attention_items(&run.id).unwrap().is_empty());
}
