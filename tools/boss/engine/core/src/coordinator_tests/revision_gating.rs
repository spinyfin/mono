//! Revision positioning and the `schedule_execution` gating ladder: chain
//! serialization, redundant-spawn and lost-workspace reconciliation, and the
//! drain-time wait-reason bookkeeping.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

/// When a `revision_implementation` execution has a non-empty `pr_url`,
/// `schedule_execution` must call `cube workspace goto` after the lease to
/// position the workspace on the PR head, and must NOT call `create_change`.
#[tokio::test]
async fn revision_with_pr_url_positions_via_goto_not_create_change() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/99";
    let (_, chore_id) = make_pr_review_fixture(&db, None);

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

    // goto_workspace must have been called with pr=99.
    let goto_calls = cube.goto_calls.lock().await;
    assert_eq!(goto_calls.len(), 1, "goto_workspace must be called exactly once");
    assert_eq!(goto_calls[0].1, 99, "goto_workspace must receive pr=99 for PR #99");
    drop(goto_calls);

    // create_change must NOT have been called — positioning happened via goto.
    assert!(
        cube.create_calls.lock().await.is_empty(),
        "create_change must not be called for the revision positioning path"
    );
}

/// Regression: a `revision_implementation` execution with `pr_url = None` (as
/// produced by the orphan-sweep re-dispatch and `bossctl work start` paths) must
/// still call `cube workspace goto` using the chain root's PR URL. Without the
/// chain-root fallback the positioning gate is silently skipped and the worker
/// lands on main instead of the PR head.
#[tokio::test]
async fn revision_without_pr_url_falls_back_to_chain_root_for_goto() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let root_pr_url = "https://github.com/spinyfin/mono/pull/88";
    // Chain root: a chore with a bound PR URL.
    let (_, root_id) = make_pr_review_fixture(&db, Some(root_pr_url));
    // Revision hanging off the chain root, created without an execution.pr_url.
    {
        let conn = db.connect().unwrap();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_no_pr_url', product_id, 'revision', 'Fix review findings', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Execution with pr_url absent — simulates orphan-sweep re-dispatch path.
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_no_pr_url")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert!(execution.pr_url.is_none(), "test precondition: pr_url must be absent");

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_ok(), "schedule_execution must succeed: {result:?}");

    // goto_workspace must have been called with pr=88 (from the chain root).
    let goto_calls = cube.goto_calls.lock().await;
    assert_eq!(
        goto_calls.len(),
        1,
        "goto_workspace must be called exactly once via chain-root fallback"
    );
    assert_eq!(
        goto_calls[0].1, 88,
        "goto_workspace must receive pr=88 from the chain root PR URL"
    );
    drop(goto_calls);

    // create_change must NOT have been called — positioning happened via goto.
    assert!(
        cube.create_calls.lock().await.is_empty(),
        "create_change must not be called when chain-root fallback positions via goto"
    );
}

/// When a `revision_implementation` lease fails (which now includes
/// When the `cube workspace lease` call fails for a `revision_implementation`
/// execution, `schedule_execution` must record a `cube_workspace_lease_failed`
/// start failure and must not have acquired a workspace to release.
#[tokio::test]
async fn revision_lease_failure_records_start_failure() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/100";
    let (_, chore_id) = make_pr_review_fixture(&db, None);

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient {
        fail_lease: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner::default());

    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_err(), "schedule_execution must fail when the lease fails");

    // No workspace was ever leased so there is nothing to release.
    assert!(
        cube.release_calls.lock().await.is_empty(),
        "no release should occur when the lease itself failed"
    );

    // A cube_workspace_lease_failed attention item must exist.
    let items = db.list_attention_items(&execution.id).unwrap();
    assert!(
        items.iter().any(|i| i.kind == "cube_workspace_lease_failed"),
        "expected a cube_workspace_lease_failed attention item, got {:?}",
        items.iter().map(|i| &i.kind).collect::<Vec<_>>(),
    );
}

/// The soft-prefer fallback must succeed when the preferred workspace is held,
/// and workspace positioning must use `goto_workspace` (not `create_change`)
/// when a `pr_url` is present.
#[tokio::test]
async fn revision_soft_prefer_fallback_positions_via_goto() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/42";
    let (_, chore_id) = make_pr_review_fixture(&db, None);

    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .preferred_workspace_id("mono-agent-001")
                .prefer_is_soft(true)
                .build(),
        )
        .unwrap();

    // Simulate the preferred workspace being held by the parent chore's worker.
    // Attempt 1 (--prefer mono-agent-001 --resume-pr 42) fails; attempt 2
    // (no --prefer, --resume-pr 42) must succeed and position the workspace.
    let cube = Arc::new(FakeCubeClient {
        fail_lease_when_prefer_set: true,
        ..FakeCubeClient::default()
    });
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    let worker_id = coordinator
        .pool_for_execution(&execution)
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(
        result.is_ok(),
        "schedule_execution must succeed via soft-prefer fallback: {result:?}"
    );

    let calls = cube.lease_calls.lock().await;
    assert_eq!(
        calls.len(),
        2,
        "two lease attempts expected: prefer failed then fallback succeeded"
    );

    // Attempt 1 targeted the preferred workspace; attempt 2 did not.
    assert_eq!(
        calls[0].2,
        Some("mono-agent-001".to_owned()),
        "attempt 1 must pass the preferred workspace"
    );
    assert_eq!(calls[1].2, None, "attempt 2 must not specify a preferred workspace");
    drop(calls);

    // Positioning happens via goto_workspace (not create_change) when pr_url is set.
    assert!(
        cube.create_calls.lock().await.is_empty(),
        "create_change must not be called when goto_workspace positions the workspace"
    );
    assert_eq!(
        cube.goto_calls.lock().await.len(),
        1,
        "goto_workspace must be called once for the PR positioning"
    );
}

/// Fix (a) regression: `schedule_execution` must refuse to dispatch a
/// `ready` execution when the work item is still gated by an unmet
/// prerequisite. Concretely: a `ready` row created via a timing race
/// (autostart flipped before the dep edge committed) must be downgraded
/// back to `waiting_dependency` and must NOT result in a cube workspace
/// lease or worker spawn.
#[tokio::test]
async fn schedule_execution_rejects_gated_ready_execution() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let product = create_test_product(&db);

    // prereq: a separate chore that has not yet completed.
    let prereq = create_test_chore(&db, product.id.clone(), "Prereq (still active)");

    // dependent: the gated chore. Created with autostart=false so no
    // execution is created automatically.
    let dep = create_test_chore_manual(&db, product.id.clone(), "Gated chore (should not dispatch)");

    // Wire the blocks edge: dep requires prereq to be done.
    db.add_dependency(AddDependencyInput {
        dependent: dep.id.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();

    // Simulate the race: directly insert a `ready` execution as if the
    // autostart flip and reconcile ran before the dep edge committed.
    let execution = create_ready_chore_execution(&db, dep.id.clone());

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&execution.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&execution, &worker_id).await;
    assert!(result.is_err(), "gated execution must be refused by schedule_execution");

    // The execution must have been downgraded to waiting_dependency, not
    // abandoned — it can be re-promoted when the gate clears.
    let updated = db.get_execution(&execution.id).unwrap();
    assert_eq!(
        updated.status,
        ExecutionStatus::WaitingDependency,
        "gated execution must be downgraded to waiting_dependency, got {:?}",
        updated.status,
    );

    // No cube calls must have been made — no workspace was leased.
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube lease must occur for a gated execution"
    );
    assert!(
        cube.ensure_calls.lock().await.is_empty(),
        "no cube ensure must occur for a gated execution"
    );
}

/// Per-PR single-writer guard (guards against the two-concurrent-writers-to-one-PR incident). When an
/// implementation execution on the chain root is live, dispatching a
/// conflict-resolution revision (a DIFFERENT work item that targets the
/// SAME PR via the chain) must be DEFERRED — not co-dispatched onto the
/// shared jj backing store, and not abandoned. The revision execution
/// must stay `ready` and must dispatch only once the live sibling reaps.
#[tokio::test]
async fn schedule_execution_defers_revision_behind_live_chain_sibling() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1467";
    // Chain root: a chore in_review with a bound PR (the implementation task).
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        // A conflict-resolution revision hanging off the chain root.
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_serialize', product_id, 'revision', 'Resolve conflicts', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Live implementation resume on the chain root.
    let root_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    // Ready conflict-resolution revision execution targeting the same PR.
    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_serialize")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&revision_exec.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&revision_exec, &worker_id).await;
    assert!(
        result.is_err(),
        "revision must be deferred while a chain sibling is live: {result:?}",
    );

    // Deferred, NOT abandoned: the execution stays `ready` so it can be
    // re-attempted when the live sibling reaps.
    let after_defer = db.get_execution(&revision_exec.id).unwrap();
    assert_eq!(
        after_defer.status,
        ExecutionStatus::Ready,
        "deferred revision must remain ready, not abandoned/waiting_dependency, got {:?}",
        after_defer.status,
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased while serialized behind a live chain sibling",
    );

    // The live sibling reaps. Mirror the drain loop releasing the worker,
    // then re-attempt: the revision must now dispatch.
    coordinator.worker_pool().release_worker(&worker_id, None).await;
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
            rusqlite::params![root_exec.id],
        )
        .unwrap();
    }

    let worker_id2 = coordinator
        .worker_pool()
        .claim_worker(&revision_exec.id, None)
        .await
        .expect("worker pool slot available after release");
    let result_after = coordinator.schedule_execution(&revision_exec, &worker_id2).await;
    assert!(
        result_after.is_ok(),
        "revision must dispatch once the live chain sibling has reaped: {result_after:?}",
    );
    assert!(
        !cube.lease_calls.lock().await.is_empty(),
        "a cube workspace must be leased once the chain is clear",
    );
}

/// The chore_18c0da77bef326b0_840 fix: a live `pr_review` sibling is
/// strictly read-only, so it must NOT chain-serialize a merge-conflict-
/// fix revision behind it — the priority inversion where the fix waited
/// the full length of a review run (2026-07-10). Unlike
/// [`schedule_execution_defers_revision_behind_live_chain_sibling`]
/// (whose live sibling is a writer and must still block), this revision
/// must dispatch immediately even though the review is still live.
#[tokio::test]
async fn schedule_execution_bypasses_live_review_sibling_for_merge_conflict_revision() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1467";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id, created_via)
                 SELECT 'task_rev_conflict_bypass', product_id, 'revision', 'Resolve conflicts', '', 'todo', '1', '1', ?1, 'merge-conflict:crz_bypass'
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Live PR review on the chain root — read-only, must not block.
    let _root_review_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    // Ready merge-conflict revision execution targeting the same PR.
    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_conflict_bypass")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&revision_exec.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&revision_exec, &worker_id).await;
    assert!(
        result.is_ok(),
        "merge-conflict revision must bypass a live read-only pr_review sibling: {result:?}",
    );
    assert!(
        !cube.lease_calls.lock().await.is_empty(),
        "a cube workspace must be leased — the review sibling must not block the lease",
    );
}

/// The correctness gap this revision closes: a live `pr_review` on the
/// chain root must NOT mask a live *writer* on a chain descendant. Sets
/// up a root review PLUS a live descendant writer (another
/// merge-conflict-fix revision execution, already running) and confirms
/// a SECOND ready merge-conflict revision on the same chain is still
/// `Blocked` (no workspace leased) — not `ReviewBypassed`. Before the
/// `resolve_chain_hold`/`live_chain_siblings` fix, the root-first single-
/// sibling walk would have returned only the review, bypassed, and
/// co-dispatched a second writer alongside the still-live descendant
/// writer — the exact two-concurrent-writers-to-one-PR hazard this guard exists to
/// prevent.
#[tokio::test]
async fn schedule_execution_blocks_conflict_revision_when_review_masks_live_descendant_writer() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1467";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id, created_via)
                 SELECT 'task_rev_conflict_writer', product_id, 'revision', 'Resolve conflicts A', '', 'todo', '1', '1', ?1, 'merge-conflict:crz_a'
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id, created_via)
                 SELECT 'task_rev_conflict_second', product_id, 'revision', 'Resolve conflicts B', '', 'todo', '1', '1', ?1, 'merge-conflict:crz_b'
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Live PR review on the chain root — read-only.
    let _root_review_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    // Live WRITER on a chain descendant — the first conflict-fix
    // revision, already dispatched and still running.
    let _descendant_writer_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_conflict_writer".to_owned())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Running)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    // A second, ready merge-conflict revision on the same chain.
    let revision_exec_b = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_conflict_second".to_owned())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&revision_exec_b.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&revision_exec_b, &worker_id).await;
    assert!(
        result.is_err(),
        "a second conflict revision must NOT bypass when a live descendant writer is masked \
             behind the root review: {result:?}",
    );
    let after_defer = db.get_execution(&revision_exec_b.id).unwrap();
    assert_eq!(
        after_defer.status,
        ExecutionStatus::Ready,
        "deferred revision must remain ready, got {:?}",
        after_defer.status,
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased while a live writer sibling exists elsewhere in the chain, \
             even if a review is also live",
    );
}

/// A non-conflict revision (no `merge-conflict:` `created_via` marker)
/// gets none of the bypass above — it keeps serializing behind a live
/// `pr_review` sibling exactly as before this fix. Only merge-conflict
/// revisions are urgent enough, and safe enough (see the module docs on
/// `ExecutionCoordinator::resolve_chain_hold`), to bypass a review.
#[tokio::test]
async fn schedule_execution_still_defers_non_conflict_revision_behind_live_review_sibling() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1467";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        // No `created_via` marker — an ordinary/operator-filed revision.
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_no_bypass', product_id, 'revision', 'Address feedback', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    let _root_review_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_no_bypass")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&revision_exec.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&revision_exec, &worker_id).await;
    assert!(
        result.is_err(),
        "a non-conflict revision must still defer behind a live pr_review sibling: {result:?}",
    );
    let after_defer = db.get_execution(&revision_exec.id).unwrap();
    assert_eq!(
        after_defer.status,
        ExecutionStatus::Ready,
        "deferred revision must remain ready, got {:?}",
        after_defer.status,
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased while serialized behind a live review sibling",
    );
}

/// The auto-dispatcher's pre-claim chain check must record a
/// `dispatch_wait_reason` that distinguishes a review-held chain hold
/// from a writer-held one — the trace line chore_18c0da77bef326b0_840
/// asked for, so an operator (or the kanban card) can tell "waiting on
/// a read-only review" from "waiting on another writer" without
/// cross-referencing `engine-trace.jsonl` by hand.
#[tokio::test]
async fn drain_records_review_held_wait_reason_distinct_from_writer_held() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/9001";
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
                 SELECT 'task_rev_wait_reason', product_id, 'revision', 'Address feedback', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    let _root_review_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_wait_reason")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    let mut wait_reason = None;
    for _ in 0..200 {
        wait_reason = db.get_execution(&revision_exec.id).unwrap().dispatch_wait_reason;
        if wait_reason.is_some() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let wait_reason = wait_reason.expect("dispatch_wait_reason must be set");
    assert!(
        wait_reason.contains("an automated PR review runs at a time"),
        "a revision deferred behind a live pr_review sibling must record the review-held \
             reason, not the generic writer-held wording; got {wait_reason:?}",
    );
    assert!(
        !wait_reason.contains("revisions on the same PR run one at a time"),
        "must not use the writer-held phrasing for a review-held wait; got {wait_reason:?}",
    );
    assert!(
        !wait_reason.to_lowercase().contains("sibling"),
        "operator-facing wait reason must not use engine-internal \"sibling\" vocabulary; \
             got {wait_reason:?}",
    );
}

/// The `chain_serialized` (writer-held, not review-held) wait reason
/// persisted into `dispatch_wait_reason` must name the concrete
/// blocking task (`T<short_id>` + name) and PR — not the opaque
/// "PR sibling" wording the opaque-sibling-wording incident (mono#1901) reported, which
/// gave an operator no way to tell what a "sibling" was, which task
/// was blocking, or which PR was involved.
#[tokio::test]
async fn drain_records_writer_held_wait_reason_names_blocking_task_and_pr() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1901";
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
                 SELECT 'task_rev_writer_held', product_id, 'revision', 'Fix failing CI', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Live writer execution on the chain root (blocks the revision below).
    let root_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let root_short_label = match db.get_work_item(&root_id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => t.short_label(),
        _ => panic!("expected a task/chore work item"),
    };

    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_writer_held")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));
    coordinator.kick();

    let mut wait_reason = None;
    for _ in 0..200 {
        wait_reason = db.get_execution(&revision_exec.id).unwrap().dispatch_wait_reason;
        if wait_reason.is_some() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }

    let wait_reason = wait_reason.expect("dispatch_wait_reason must be set");
    assert!(
        wait_reason.contains(&root_short_label),
        "wait reason must name the blocking sibling's T-id; got {wait_reason:?}",
    );
    assert!(
        wait_reason.contains("Test chore"),
        "wait reason must name the blocking sibling's task title; got {wait_reason:?}",
    );
    assert!(
        wait_reason.contains("mono#1901"),
        "wait reason must name the blocking PR; got {wait_reason:?}",
    );
    assert!(
        wait_reason.contains("revisions on the same PR run one at a time"),
        "writer-held wait must not use the review-held phrasing; got {wait_reason:?}",
    );
    assert!(
        !wait_reason.to_lowercase().contains("sibling"),
        "operator-facing wait reason must not use engine-internal \"sibling\" vocabulary; \
             got {wait_reason:?}",
    );
    assert!(root_exec.id != revision_exec.id, "sanity: distinct executions");
}

/// Redundant-spawn guard emits `host_selected:error` with `reason=redundant_spawn`
/// when a live execution already exists for the same work item. The execution is
/// marked abandoned (not left ready), and the stall watchdog must not fire because
/// the terminal `host_selected:error` event closes the stage immediately.
#[tokio::test]
async fn redundant_spawn_guard_emits_terminal_host_selected_error() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Redundant-spawn chore");

    // Simulate the race: a first execution is already live (running).
    let _live_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    // A second, redundant `ready` execution arrives (the one under test).
    let redundant_exec = create_ready_chore_execution(&db, chore.id.clone());

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&redundant_exec.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&redundant_exec, &worker_id).await;
    assert!(result.is_err(), "redundant spawn must be rejected: {result:?}");

    // Dispatch timeline: must carry a terminal host_selected:error with the right reason.
    let events = recording.events_for(&redundant_exec.id).await;
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
    assert_eq!(
        host_selected.outcome, "error",
        "redundant_spawn must surface as host_selected:error; got {host_selected:?}",
    );
    assert_eq!(
        host_selected.details.get("reason").and_then(|v| v.as_str()),
        Some("redundant_spawn"),
        "host_selected:error must name redundant_spawn reason; got {:?}",
        host_selected.details,
    );
    assert!(
        crate::dispatch_reader::is_terminal_event(host_selected),
        "host_selected:error must be terminal so the stall watchdog never fires",
    );

    // No stage_stalled must appear — the terminal event closes the stage immediately.
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        !stages.contains(&"stage_stalled"),
        "redundant_spawn must not produce a stage_stalled event; got {stages:?}",
    );

    // Post-condition: the redundant execution is abandoned, not left in a live state.
    let after = db.get_execution(&redundant_exec.id).unwrap();
    assert_eq!(
        after.status,
        ExecutionStatus::Abandoned,
        "redundant execution must be marked abandoned; got {:?}",
        after.status,
    );

    // No cube workspace was touched.
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased for a redundant spawn",
    );
}

/// Liveness gate (2026-06-14 waiting_human-zombie fix): when the "live"
/// blocker the redundant-spawn guard finds is actually a zombie — a local
/// execution whose recorded cube workspace directory has vanished (its
/// pane is gone) — the guard must reconcile it to a terminal status and
/// let the new spawn PROCEED, rather than rejecting it forever as
/// redundant. This is the exact wedge that broke all automations for 17
/// days: three triage rows stuck `waiting_human` after their workspaces
/// were migrated away blocked every subsequent fire with `redundant_spawn`.
#[tokio::test]
async fn redundant_spawn_guard_reconciles_lost_workspace_zombie_and_proceeds() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Zombie-blocked chore");

    // The "live" blocker: a running execution with a real run on the local
    // host, but whose workspace directory no longer exists on disk — a
    // dead pane the engine never reaped (no Stop hook).
    let zombie = create_ready_chore_execution(&db, chore.id.clone());
    db.start_execution_run(
        &zombie.id,
        "worker-1",
        "repo-1",
        "lease-1",
        "mono-agent-028",
        "/nonexistent/old-root/mono-agent-028",
    )
    .unwrap();
    // Park it in waiting_human, exactly like a just-spawned worker.
    let zrun = db
        .active_run_ids_for_execution(&zombie.id)
        .unwrap()
        .into_iter()
        .next()
        .expect("zombie has a run");
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&zombie.id)
            .run_id(&zrun)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .build(),
    )
    .unwrap();

    // The new execution the scheduler wants to spawn.
    let fresh = create_ready_chore_execution(&db, chore.id.clone());

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&fresh.id, None)
        .await
        .expect("worker pool slot available");

    let _ = coordinator.schedule_execution(&fresh, &worker_id).await;

    // The zombie was reconciled to a terminal status (orphaned) with a
    // lost_workspace_reconcile trace event naming its prior status.
    let zombie_after = db.get_execution(&zombie.id).unwrap();
    assert_eq!(
        zombie_after.status,
        ExecutionStatus::Orphaned,
        "the lost-workspace zombie must be finalized; got {:?}",
        zombie_after.status,
    );
    let zombie_events = recording.events_for(&zombie.id).await;
    let reconcile = zombie_events
        .iter()
        .find(|e| e.stage == "lost_workspace_reconcile")
        .unwrap_or_else(|| panic!("expected lost_workspace_reconcile event; got {zombie_events:#?}"));
    assert_eq!(
        reconcile.details.get("prior_status").and_then(|v| v.as_str()),
        Some("waiting_human"),
        "the trace must record the prior status; got {:?}",
        reconcile.details,
    );

    // The fresh execution was NOT rejected as redundant.
    let fresh_after = db.get_execution(&fresh.id).unwrap();
    assert_ne!(
        fresh_after.status,
        ExecutionStatus::Abandoned,
        "the new spawn must not be abandoned as redundant once the zombie is cleared",
    );
    let fresh_events = recording.events_for(&fresh.id).await;
    assert!(
        !fresh_events.iter().any(|e| e.stage == "host_selected"
            && e.details.get("reason").and_then(|v| v.as_str()) == Some("redundant_spawn")),
        "the new spawn must not emit a redundant_spawn host_selected:error; got {fresh_events:#?}",
    );
}

/// Chain-serialized backstop guard emits `host_selected:error` with
/// `reason=chain_serialized_backstop` when a live execution exists on a
/// different work item in the same revision chain. This guard is the backstop
/// for `force_dispatch` (the auto-dispatcher pre-filters at the worker-claim
/// stage). The deferred execution must stay `ready` so it can be re-dispatched
/// once the live sibling reaps; it must NOT be abandoned. The stall watchdog
/// must not fire because the terminal event closes the stage immediately.
#[tokio::test]
async fn chain_serialized_backstop_emits_terminal_host_selected_error() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1849";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        // Revision hanging off the chain root (same PR, different work item).
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_backstop', product_id, 'revision', 'Resolve conflicts backstop', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Live implementation resume on the chain root (blocks the revision).
    let _root_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();

    // Ready conflict-resolution revision execution targeting the same PR.
    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_backstop")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    // Use force_dispatch to hit the schedule_execution backstop (the auto-dispatcher
    // pre-filters this case before claiming a worker, so force_dispatch is the path
    // that actually reaches this guard in production).
    let result = coordinator.force_dispatch(&revision_exec.id).await;
    assert!(
        result.is_err(),
        "chain-serialized revision must be deferred: {result:?}",
    );

    // Dispatch timeline: must carry a terminal host_selected:error with the right reason.
    let events = recording.events_for(&revision_exec.id).await;
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
    assert_eq!(
        host_selected.outcome, "error",
        "chain_serialized_backstop must surface as host_selected:error; got {host_selected:?}",
    );
    assert_eq!(
        host_selected.details.get("reason").and_then(|v| v.as_str()),
        Some("chain_serialized_backstop"),
        "host_selected:error must name chain_serialized_backstop reason; got {:?}",
        host_selected.details,
    );
    assert!(
        crate::dispatch_reader::is_terminal_event(host_selected),
        "host_selected:error must be terminal so the stall watchdog never fires",
    );

    // No stage_stalled — the terminal event closes the stage immediately.
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        !stages.contains(&"stage_stalled"),
        "chain_serialized_backstop must not produce stage_stalled; got {stages:?}",
    );

    // Post-condition: deferred, NOT abandoned — execution stays ready for re-dispatch.
    let after = db.get_execution(&revision_exec.id).unwrap();
    assert_eq!(
        after.status,
        ExecutionStatus::Ready,
        "chain-serialized execution must stay ready (deferred, not abandoned); got {:?}",
        after.status,
    );

    // No cube workspace was touched.
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased while serialized behind a live chain sibling",
    );
}

/// Chain-serialization re-defer stall incident (2026-07-09, `exec_18af40745c552070_26`): the chain
/// single-writer guard must not treat a `waiting_human` chain sibling as
/// live forever when its worker pane is actually dead (workspace
/// directory gone, no `Stop` hook). The auto-dispatcher's pre-claim
/// `chain_serialized` check — the path that actually looped every ~10s
/// in the incident — must reconcile the zombie via the same
/// `lost_workspace_sweep` logic the double-spawn guard already uses, and
/// let the ready revision dispatch instead of deferring indefinitely.
#[tokio::test]
async fn chain_serialized_pre_claim_reconciles_lost_workspace_zombie_and_dispatches() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1852";
    let (_, root_id) = make_pr_review_fixture(&db, Some(pr_url));
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![root_id],
        )
        .unwrap();
        // Revision hanging off the chain root (same PR, different work item).
        conn.execute(
                "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
                 SELECT 'task_rev_t251', product_id, 'revision', 'Resolve merge conflict against main', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // The chain root's execution: a `waiting_human` zombie whose recorded
    // workspace directory no longer exists — exactly the 56-day-old
    // `exec_18af40745c552070_26` from the incident (a dead pane, no `Stop`
    // hook, no live pane per `bossctl agents status`).
    let root_exec = create_ready_chore_execution(&db, root_id.clone());
    db.start_execution_run(
        &root_exec.id,
        "worker-1",
        "repo-1",
        "lease-1",
        "mono-agent-t251",
        "/nonexistent/old-root/mono-agent-t251",
    )
    .unwrap();
    let root_run = db
        .active_run_ids_for_execution(&root_exec.id)
        .unwrap()
        .into_iter()
        .next()
        .expect("root has a run");
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&root_exec.id)
            .run_id(&root_run)
            .execution_status(ExecutionStatus::WaitingHuman)
            .run_status("completed")
            .build(),
    )
    .unwrap();

    // Ready conflict-resolution revision execution targeting the same PR —
    // this is the row that looped `chain_serialized` every ~10s in the
    // incident.
    let revision_exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id("task_rev_t251")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    // Drive the real auto-dispatch path (not `force_dispatch`), since the
    // incident's silent 10s loop was the pre-claim guard in
    // `drain_ready_queue`, not the `schedule_execution` backstop.
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &revision_exec.id, ExecutionStatus::Running).await;

    // The zombie chain root was reconciled to a terminal status, not left
    // wedging every future dispatch behind it.
    let root_after = db.get_execution(&root_exec.id).unwrap();
    assert_eq!(
        root_after.status,
        ExecutionStatus::Orphaned,
        "the lost-workspace zombie chain sibling must be finalized; got {:?}",
        root_after.status,
    );

    // No lingering `chain_serialized` deferrals once the zombie clears.
    let revision_events = recording.events_for(&revision_exec.id).await;
    assert!(
        !revision_events.iter().any(
            |e| e.details.get("reason").and_then(|v| v.as_str()) == Some("chain_serialized")
                && e.stage == "worker_claimed"
        ),
        "the revision must not be permanently deferred behind a dead sibling; got {revision_events:#?}",
    );
}

/// Part (c) of the chain-serialization re-defer stall fix: once a `ready` execution has sat
/// chain-serialized behind a genuinely live sibling for longer than
/// [`CHAIN_SERIALIZED_STALL_THRESHOLD_SECS`], a durable, user-visible
/// `chain_serialized_stall` attention must be raised on its work item —
/// the incident sat in this exact state for ~20 minutes with the only
/// signal being a `engine-trace.jsonl` line repeating every ~10s.
#[tokio::test]
async fn chain_serialized_stall_raises_durable_attention_after_threshold() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let pr_url = "https://github.com/spinyfin/mono/pull/1853";
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
                 SELECT 'task_rev_stall', product_id, 'revision', 'Resolve merge conflict', '', 'todo', '1', '1', ?1
                 FROM tasks WHERE id = ?1",
                rusqlite::params![root_id],
            )
            .unwrap();
    }

    // Genuinely live root execution: no run attached, so the zombie
    // reconcilers have zero evidence either way and correctly leave it
    // alone (this must stay `chain_serialized`, not get reconciled away).
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
                .work_item_id("task_rev_stall")
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .pr_url(pr_url.to_owned())
                .build(),
        )
        .unwrap();

    // Backdate `created_at` well past the stall threshold — simulates the
    // chain-serialization re-defer stall incident's ~20 minutes of silent re-defers without a real sleep.
    {
        let conn = db.connect().unwrap();
        let stale_created_at =
            (boss_engine_utils::epoch_time::now_epoch_secs() - CHAIN_SERIALIZED_STALL_THRESHOLD_SECS - 60).to_string();
        conn.execute(
            "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![revision_exec.id, stale_created_at],
        )
        .unwrap();
    }

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    coordinator.kick();

    let mut items = Vec::new();
    for _ in 0..100 {
        items = db.list_attention_items_for_work_item("task_rev_stall").unwrap();
        if !items.is_empty() {
            break;
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(items.len(), 1, "expected exactly one attention item; got {items:#?}");
    assert_eq!(items[0].kind, CHAIN_SERIALIZED_STALL_ATTENTION_KIND);
    assert_eq!(items[0].status, "open");

    // Still `ready` (deferred, not abandoned) — the root is genuinely alive.
    let after = db.get_execution(&revision_exec.id).unwrap();
    assert_eq!(
        after.status,
        ExecutionStatus::Ready,
        "a stall attention must not itself change the execution's status; got {:?}",
        after.status,
    );
}

/// Gating-prereqs guard emits `host_selected:error` with
/// `reason=gating_prereqs_blocked` when the work item has an unmet
/// prerequisite at dispatch time. The execution must be downgraded to
/// `waiting_dependency` (not abandoned) so it can be re-promoted when the
/// gate clears. The stall watchdog must not fire because the terminal event
/// closes the stage immediately.
#[tokio::test]
async fn gating_prereqs_guard_emits_terminal_host_selected_error() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let product = create_test_product(&db);

    // prereq: a chore that has not yet completed.
    let prereq = create_test_chore(&db, product.id.clone(), "Prereq (still active)");

    // dependent: the gated chore.
    let dep = create_test_chore_manual(&db, product.id.clone(), "Gated chore");

    // Wire the blocks edge: dep requires prereq to be done first.
    db.add_dependency(AddDependencyInput {
        dependent: dep.id.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();

    // Simulate the timing race: a `ready` execution created before the dep edge committed.
    let gated_exec = create_ready_chore_execution(&db, dep.id.clone());

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&gated_exec.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&gated_exec, &worker_id).await;
    assert!(result.is_err(), "gated execution must be refused: {result:?}");

    // Dispatch timeline: must carry a terminal host_selected:error with the right reason.
    let events = recording.events_for(&gated_exec.id).await;
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
    assert_eq!(
        host_selected.outcome, "error",
        "gating_prereqs_blocked must surface as host_selected:error; got {host_selected:?}",
    );
    assert_eq!(
        host_selected.details.get("reason").and_then(|v| v.as_str()),
        Some("gating_prereqs_blocked"),
        "host_selected:error must name gating_prereqs_blocked reason; got {:?}",
        host_selected.details,
    );
    assert!(
        crate::dispatch_reader::is_terminal_event(host_selected),
        "host_selected:error must be terminal so the stall watchdog never fires",
    );

    // No stage_stalled — the terminal event closes the stage immediately.
    let stages: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        !stages.contains(&"stage_stalled"),
        "gating_prereqs_blocked must not produce stage_stalled; got {stages:?}",
    );

    // Post-condition: downgraded to waiting_dependency (not abandoned) so it can
    // be re-promoted when the prerequisite clears.
    let after = db.get_execution(&gated_exec.id).unwrap();
    assert_eq!(
        after.status,
        ExecutionStatus::WaitingDependency,
        "gated execution must be downgraded to waiting_dependency; got {:?}",
        after.status,
    );

    // No cube workspace was touched.
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased for a gated execution",
    );
}

/// 2026-07-03 incident (exec_18be836b10baae8_35): a merge-conflict
/// revision can sit `ready` behind worker-pool contention while the
/// periodic merge-poller sweep (`conflict_watch::on_resolved`) notices the
/// bound PR is mergeable again and independently retires the linked
/// `conflict_resolutions` ledger row to `succeeded`. Dispatching a worker
/// for this now-unnecessary revision would just have it discover "nothing
/// to do" and churn the produce-a-PR nudge loop. The dispatch-time guard
/// must catch this before ever leasing a workspace: retire the execution
/// and advance the revision task to `in_review` without spawning anyone.
#[tokio::test]
async fn merge_conflict_already_resolved_guard_short_circuits_dispatch() {
    use crate::work::{ConflictResolutionInsertInput, FakePrStateChecker, PrOpenState};
    use boss_protocol::CreateRevisionInput;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());

    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Boss")
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .build(),
        )
        .unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/1709";
    let parent = create_test_chore_manual(&db, product.id.clone(), "Parent chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![parent.id, parent_pr_url],
        )
        .unwrap();
    }
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.id.clone(),
            work_item_id: parent.id.clone(),
            pr_url: parent_pr_url.to_owned(),
            pr_number: 1709,
            head_branch: "my-feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base_sha_1".into()),
            head_sha_before: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        })
        .unwrap()
        .unwrap();
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(parent.id.clone())
                .description("Resolve merge conflict against main")
                .created_via(format!("merge-conflict:{}", attempt.id))
                .build(),
            &checker,
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
        .unwrap();
    // The periodic merge-poller sweep already found the PR mergeable and
    // retired the ledger row independent of any worker.
    db.mark_conflict_resolution_succeeded(&attempt.id, None).unwrap();

    let exec = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(revision.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:spinyfin/mono.git")
                .pr_url(parent_pr_url)
                .build(),
        )
        .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone())
            .with_pre_start_retry_delays(Vec::new()),
    );

    let worker_id = coordinator
        .worker_pool()
        .claim_worker(&exec.id, None)
        .await
        .expect("worker pool slot available");

    let result = coordinator.schedule_execution(&exec, &worker_id).await;
    assert!(
        result.is_err(),
        "an already-resolved merge-conflict revision must not be dispatched: {result:?}"
    );

    let events = recording.events_for(&exec.id).await;
    let host_selected = events
        .iter()
        .find(|e| e.stage == "host_selected")
        .unwrap_or_else(|| panic!("expected host_selected event; got {events:#?}"));
    assert_eq!(
        host_selected.outcome, "error",
        "already-resolved guard must surface as host_selected:error; got {host_selected:?}",
    );
    assert_eq!(
        host_selected.details.get("reason").and_then(|v| v.as_str()),
        Some("merge_conflict_already_resolved"),
        "host_selected:error must name merge_conflict_already_resolved; got {:?}",
        host_selected.details,
    );

    // Execution retired without ever leasing a workspace.
    let after_exec = db.get_execution(&exec.id).unwrap();
    assert_eq!(
        after_exec.status,
        ExecutionStatus::Abandoned,
        "execution must be abandoned, not dispatched; got {:?}",
        after_exec.status,
    );
    assert!(
        cube.lease_calls.lock().await.is_empty(),
        "no cube workspace may be leased for an already-resolved revision",
    );

    // Revision task advances to in_review — same terminal state a normal
    // completed revision reaches — instead of stranding in `active`.
    match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Task(t) | WorkItem::Chore(t) => assert_eq!(
            t.status,
            TaskStatus::InReview,
            "revision task must leave `active` when the conflict resolved before dispatch",
        ),
        other => panic!("expected task, got {other:?}"),
    }
}

/// Occupancy-guard livelock regression. When cube keeps handing
/// back the same occupied workspace, the engine must exclude it on the
/// next lease call and land on a different free workspace.
///
/// Setup:
///   - mono-agent-037 is returned first by cube, and the live-worker
///     registry says exec-live occupies it (live pid = current test
///     process, so probe_pid sees it as alive).
///   - mono-agent-014 is returned on subsequent calls (simulating cube
///     respecting --exclude mono-agent-037).
///
/// Expected: the execution lands on mono-agent-014, not loops forever
/// on mono-agent-037; and the second lease call carries --exclude
/// mono-agent-037.
#[tokio::test]
async fn occupancy_refusal_excludes_workspace_on_retry() {
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use boss_protocol::WorkerEvent;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Anti-livelock task");
    db.reconcile_product_executions(&product.id).unwrap();

    // Create a second product and chore to serve as the "occupied" execution.
    // We need a separate DB row for exec-live so get_execution(exec_live.id)
    // returns cube_workspace_id = "mono-agent-037".
    let other_product = create_test_product_named(&db, "OtherProduct");
    let other_chore = create_test_chore(&db, other_product.id.clone(), "Live worker chore");
    db.reconcile_product_executions(&other_product.id).unwrap();
    let exec_live = db.list_executions(Some(&other_chore.id)).unwrap().pop().unwrap();
    // Transition it to running so start_execution_run can set cube_workspace_id.
    db.start_execution_run(
        &exec_live.id,
        "worker-0",
        "mono",
        "lease-live",
        "mono-agent-037",
        "/workspaces/mono-agent-037",
    )
    .unwrap();

    // First lease returns the occupied workspace; subsequent calls
    // return the free one (simulating cube respecting --exclude).
    let cube = Arc::new(FakeCubeClient::default().with_workspace_id_queue(["mono-agent-037", "mono-agent-014"]));

    // Wire the live-worker registry: exec-live is Working and alive
    // (pid = current test process → probe_pid sees it as alive).
    let live_pid = std::process::id() as i32;
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    live_states.register_spawn(0, &exec_live.id, "sonnet", live_pid, None);
    // Advance the slot to Working so the occupancy guard doesn't see Spawning.
    live_states.apply_event(
        0,
        &WorkerEvent::UserPromptSubmit {
            session_id: "s".to_owned(),
            prompt: "do the thing".to_owned(),
        },
    );

    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });

    let mut coordinator_inner = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coordinator_inner.set_live_worker_states(live_states);
    // Zero-delay retry so the test doesn't sleep.
    let coordinator = Arc::new(coordinator_inner.with_pre_start_retry_delays(vec![Duration::ZERO]));

    coordinator.kick();

    // Wait for the anti-livelock task execution to reach Running.
    let our_execution_id = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap().id;

    wait_for_execution_status(db.as_ref(), &our_execution_id, ExecutionStatus::Running).await;

    let calls = cube.lease_calls.lock().await;
    // At minimum two cube lease calls: first returning mono-agent-037
    // (refused by occupancy guard), then returning mono-agent-014 (accepted).
    assert!(
        calls.len() >= 2,
        "expected at least 2 lease calls (refused + accepted); got {}",
        calls.len()
    );
    // The second call must exclude mono-agent-037.
    assert!(
        calls[1].4.iter().any(|id| id == "mono-agent-037"),
        "second lease call must pass --exclude mono-agent-037; got {:?}",
        calls[1].4
    );
    drop(calls);

    // The execution must have landed on mono-agent-014, not 037.
    let execution = db.get_execution(&our_execution_id).unwrap();
    assert_eq!(
        execution.cube_workspace_id.as_deref(),
        Some("mono-agent-014"),
        "execution must land on the free workspace, not the occupied one"
    );
}
