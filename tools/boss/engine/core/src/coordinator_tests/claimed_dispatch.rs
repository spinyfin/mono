//! Drain-loop and failure-recovery behaviour of the `claimed` dispatch CAS.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;
use boss_protocol::ExecutionKind;

/// Drain-loop chain hold must revert the pickup claim so a serialized row
/// stays `ready` rather than stranding in `claimed`.
#[tokio::test]
async fn drain_chain_hold_leaves_row_ready_not_claimed() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/2823";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             SELECT 'task_rev_claim_hold', product_id, 'revision', 'Fix review findings', '', 'todo', '1', '1', ?1
             FROM tasks WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
    }

    let _root_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_claim_hold")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    let mut wait_reason = None;
    for _ in 0..200 {
        let execution = db.get_execution(&revision_exec.id).unwrap();
        wait_reason = execution.dispatch_wait_reason.clone();
        if wait_reason.is_some() {
            assert_eq!(
                execution.status,
                ExecutionStatus::Ready,
                "chain-hold deferral must revert the drain claim"
            );
            assert_eq!(
                execution.pre_start_failure_count, 0,
                "chain-serialized deferral must not count as a spawn failure"
            );
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(wait_reason.is_some(), "drain must stamp a chain-serialized wait reason");
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "chain hold must not lease a workspace"
    );
}

/// Merge-order stagger must stamp `dispatch_not_before` on a drain-claimed
/// row and Drop must return the row to `ready`.
#[tokio::test]
async fn drain_stagger_stamps_dispatch_not_before_and_returns_to_ready() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let first = create_test_chore(&db, product.id.clone(), "First");
    let later = create_test_chore(&db, product.id.clone(), "Later");
    {
        let conn = db.connect().unwrap();
        crate::work_dependencies::insert_edge(
            &conn,
            &later.id,
            &first.id,
            crate::work_dependencies::RELATION_MERGE_ORDER,
            "1",
        )
        .unwrap();
    }

    let later_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(later.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .priority(100)
                .build(),
        )
        .unwrap();

    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coordinator.set_merge_order_stagger_secs(60);
    let coordinator = Arc::new(coordinator);
    coordinator.kick();

    for _ in 0..200 {
        let execution = db.get_execution(&later_exec.id).unwrap();
        if execution.dispatch_not_before.is_some() {
            assert_eq!(
                execution.status,
                ExecutionStatus::Ready,
                "stagger deferral must revert the drain claim; got {:?}",
                execution.status
            );
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "stagger never stamped dispatch_not_before; status={:?}",
        db.get_execution(&later_exec.id).unwrap().status
    );
}

/// `recover_failed_dispatch` records a pre-start failure on a still-claimed
/// row and returns it to `ready`.
#[tokio::test]
async fn recover_failed_dispatch_records_pre_start_failure_and_unclaims() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();
    assert_eq!(
        db.claim_execution_for_dispatch(&execution_id).unwrap(),
        crate::work::DispatchClaimOutcome::Won
    );
    let execution = db.get_execution(&execution_id).unwrap();

    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(1),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner::default()),
        )
        .with_pre_start_retry_delays(vec![Duration::from_secs(30)]),
    );
    coordinator
        .recover_failed_dispatch(&execution, "worker-1", &anyhow!("cube setup exploded"))
        .await;

    let after = db.get_execution(&execution_id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Ready);
    assert_eq!(after.pre_start_failure_count, 1);
}

/// A panic inside the detached spawn must not leave the row `claimed`.
#[tokio::test]
async fn spawn_panic_during_lease_reverts_claimed_to_ready() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Cleanup");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution_id = db.list_executions(Some(&chore.id)).unwrap()[0].id.clone();

    let cube = Arc::new(FakeCubeClient {
        panic_on_next_lease: AtomicBool::new(true),
        ..FakeCubeClient::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    ));
    coordinator.kick();

    for _ in 0..200 {
        let status = db.get_execution(&execution_id).unwrap().status;
        if status == ExecutionStatus::Ready {
            assert_eq!(
                db.get_execution(&execution_id).unwrap().pre_start_failure_count,
                0,
                "a panicked spawn must unclaim without counting a pre-start failure"
            );
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "claimed row was not reverted after spawn panic; status={:?}",
        db.get_execution(&execution_id).unwrap().status
    );
}
