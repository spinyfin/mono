//! A revision must not be dispatched onto a PR chain while the engine is
//! rewriting that PR's branch in the shared jj store.

use super::helpers::*;

#[tokio::test]
async fn a_ready_revision_waits_while_the_engine_rebases_its_chain() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let pr_url = "https://github.com/spinyfin/mono/pull/3486";
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
             SELECT 'task_rev_rebase_hold', product_id, 'revision', 'Fix failing CI', '', 'todo', '1', '1', ?1
             FROM tasks WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
    }
    let revision = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_rebase_hold")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    // The ladder holds the chain: no live worker, so the hold is granted.
    let hold = db.try_hold_chain_for_engine_rebase(&root_id).expect("no live writers");

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();
    let mut wait_reason = None;
    for _ in 0..200 {
        wait_reason = db.get_execution(&revision.id).unwrap().dispatch_wait_reason;
        if wait_reason.is_some() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        wait_reason.is_some(),
        "the revision must be deferred behind the engine rebase"
    );
    assert_eq!(
        db.get_execution(&revision.id).unwrap().status,
        ExecutionStatus::Ready,
        "the revision must stay queued while the chain is held"
    );
    assert!(
        runner.calls.lock().await.is_empty(),
        "no worker may start during the rebase"
    );
    assert!(
        db.try_hold_chain_for_engine_rebase(&root_id).is_err(),
        "a second engine rebase of the held chain is refused"
    );

    // Releasing the hold lets the queued revision through.
    drop(hold);
    coordinator.kick();
    for _ in 0..300 {
        if !runner.calls.lock().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !runner.calls.lock().await.is_empty(),
        "the revision dispatches once the engine rebase finishes"
    );
}
