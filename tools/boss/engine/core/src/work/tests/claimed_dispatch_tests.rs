use super::*;

use std::time::Duration;

/// Two incomplete project tasks: the reconciler's ordinal-chain rule makes
/// only the first `ready` and forces the second to `waiting_dependency`,
/// even when the second has no `blocks` prerequisites. That disagreement
/// with the dispatcher is what made the ready-window race reachable.
fn two_incomplete_project_tasks(db: &WorkDb) -> (String, Task, Task) {
    let product = create_test_product(db);
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Claimed dispatch project".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: false,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let first = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("First")
                .build(),
        )
        .unwrap();
    let second = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Second")
                .build(),
        )
        .unwrap();
    complete_design_for_project(db, &project.id);
    (product.id, first, second)
}

#[test]
fn claim_execution_for_dispatch_is_a_ready_to_claimed_cas() {
    let db = WorkDb::open(temp_db_path("claim-cas")).unwrap();
    let (product_id, first, _) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();

    assert_eq!(
        db.claim_execution_for_dispatch(&execution_id).unwrap(),
        DispatchClaimOutcome::Won
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Claimed
    );
    assert_eq!(
        db.claim_execution_for_dispatch(&execution_id).unwrap(),
        DispatchClaimOutcome::AlreadyHeld
    );
}

#[test]
fn claim_execution_for_dispatch_rejects_waiting_dependency() {
    let db = WorkDb::open(temp_db_path("claim-reject-waiting")).unwrap();
    let (product_id, _, second) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.list_executions(Some(&second.id)).unwrap()[0].id.clone();
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );
    assert_eq!(
        db.claim_execution_for_dispatch(&execution_id).unwrap(),
        DispatchClaimOutcome::Rejected
    );
}

#[test]
fn release_dispatch_claim_reverts_claimed_to_ready() {
    let db = WorkDb::open(temp_db_path("release-claim")).unwrap();
    let (product_id, first, _) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    db.claim_execution_for_dispatch(&execution_id).unwrap();
    assert!(db.release_dispatch_claim(&execution_id).unwrap());
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(!db.release_dispatch_claim(&execution_id).unwrap());
}

#[test]
fn reconciler_does_not_overwrite_a_claimed_later_ordinal_project_task() {
    let db = WorkDb::open(temp_db_path("reconcile-claimed")).unwrap();
    let (product_id, first, second) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();

    let first_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    let second_id = db.list_executions(Some(&second.id)).unwrap()[0].id.clone();
    assert_eq!(db.get_execution(&first_id).unwrap().status, ExecutionStatus::Ready);
    assert_eq!(
        db.get_execution(&second_id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );

    // Dispatcher treated this row as independently ready (empty prerequisite
    // list) and picked it up. The ordinal-chain reconciler still wants it
    // `waiting_dependency`.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'ready' WHERE id = ?1",
            [&second_id],
        )
        .unwrap();
    }
    assert_eq!(
        db.claim_execution_for_dispatch(&second_id).unwrap(),
        DispatchClaimOutcome::Won
    );

    let result = db.reconcile_product_executions(&product_id).unwrap();
    assert!(
        result.updated.iter().all(|e| e.id != second_id),
        "reconciler must not clobber a claimed spawn; updated={:?}",
        result
            .updated
            .iter()
            .map(|e| (&e.id, e.status.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        db.get_execution(&second_id).unwrap().status,
        ExecutionStatus::Claimed,
        "claimed execution must survive the ordinal-chain waiting_dependency flip"
    );
    assert_eq!(db.get_execution(&first_id).unwrap().status, ExecutionStatus::Ready);
}

#[test]
fn start_execution_run_accepts_claimed_and_still_rejects_waiting_dependency() {
    let db = WorkDb::open(temp_db_path("start-from-claimed")).unwrap();
    let (product_id, first, second) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let claimed_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    let waiting_id = db.list_executions(Some(&second.id)).unwrap()[0].id.clone();
    db.claim_execution_for_dispatch(&claimed_id).unwrap();

    let (execution, _run) = db
        .start_execution_run(&claimed_id, "worker-1", "repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    assert_eq!(execution.status, ExecutionStatus::Running);

    let err = db
        .start_execution_run(&waiting_id, "worker-1", "repo", "lease", "ws", "/tmp/ws")
        .unwrap_err();
    assert!(
        err.to_string().contains("waiting_dependency"),
        "readiness guard must still reject waiting_dependency; got {err}"
    );
}

#[test]
fn record_pre_start_failure_from_claimed_retries_as_ready() {
    let db = WorkDb::open(temp_db_path("pre-start-from-claimed")).unwrap();
    let (product_id, first, second) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let claimed_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    let waiting_id = db.list_executions(Some(&second.id)).unwrap()[0].id.clone();
    db.claim_execution_for_dispatch(&claimed_id).unwrap();

    let (execution, run, outcome) = db
        .record_pre_start_failure(
            &claimed_id,
            "worker-1",
            Some("repo"),
            "cube lease failed after claim",
            &[Duration::from_secs(5)],
        )
        .unwrap();
    assert!(run.is_none());
    assert!(matches!(outcome, PreStartFailureOutcome::Retry { .. }));
    assert_eq!(execution.status, ExecutionStatus::Ready);
    assert_eq!(execution.pre_start_failure_count, 1);

    let err = db
        .record_pre_start_failure(
            &waiting_id,
            "worker-1",
            None,
            "should not record",
            &[Duration::from_secs(5)],
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("waiting_dependency"),
        "pre-start-failure guard must still reject waiting_dependency; got {err}"
    );
}

#[test]
fn downgrade_ready_to_waiting_dependency_accepts_claimed() {
    let db = WorkDb::open(temp_db_path("downgrade-claimed")).unwrap();
    let (product_id, first, _) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    db.claim_execution_for_dispatch(&execution_id).unwrap();
    assert!(db.downgrade_ready_to_waiting_dependency(&execution_id).unwrap());
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingDependency
    );
}

#[test]
fn request_execution_reuses_claimed_row_when_live_oracle_is_false() {
    let db = WorkDb::open(temp_db_path("request-claimed-reuse")).unwrap();
    let (product_id, first, _) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    assert_eq!(
        db.claim_execution_for_dispatch(&execution_id).unwrap(),
        DispatchClaimOutcome::Won
    );

    let reused = db
        .request_execution_with_live_check(
            RequestExecutionInput::builder().work_item_id(first.id.clone()).build(),
            |_| false,
        )
        .unwrap();

    assert_eq!(
        reused.id, execution_id,
        "a claimed row with no work_runs must be reused, not abandoned"
    );
    assert_eq!(reused.status, ExecutionStatus::Claimed);
    let rows = db.list_executions(Some(&first.id)).unwrap();
    assert_eq!(rows.len(), 1, "must not mint a duplicate ready execution: {rows:?}");
    assert_eq!(rows[0].status, ExecutionStatus::Claimed);
}

#[test]
fn reconcile_does_not_create_duplicate_execution_for_claimed_revision() {
    let db = WorkDb::open(temp_db_path("revision-no-dup-claimed")).unwrap();
    let product_id = make_revision_product(&db, "no-dup-claimed");
    let pr_url = "https://github.com/spinyfin/mono/pull/2823";
    let parent_id = make_in_review_chore(&db, &product_id, pr_url);
    let revision_id = insert_revision_row(&db, &product_id, &parent_id);

    db.reconcile_product_executions(&product_id).unwrap();
    let execs = executions_for(&db, &revision_id);
    assert_eq!(execs.len(), 1, "first reconcile must create exactly one execution");
    assert_eq!(execs[0].1, "ready");
    assert_eq!(
        db.claim_execution_for_dispatch(&execs[0].0).unwrap(),
        DispatchClaimOutcome::Won
    );

    db.reconcile_product_executions(&product_id).unwrap();
    let after = executions_for(&db, &revision_id);
    assert_eq!(
        after.len(),
        1,
        "reconcile must not mint a duplicate while a claimed revision spawn is in flight: {after:?}"
    );
    assert_eq!(after[0].1, "claimed");
}

#[test]
fn merge_cancel_reconcile_does_not_mint_over_an_older_live_execution() {
    let db = WorkDb::open(temp_db_path("merge-cancel-live-guard")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Continue merged review");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET kind = 'followup', status = 'todo', autostart = 1 WHERE id = ?1",
            [&chore.id],
        )
        .unwrap();
    }
    let live = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let cancelled = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::Cancelled)
                .preferred_workspace_id("mono-agent-003")
                .build(),
        )
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '2026-09-02T00:00:00Z' WHERE id = ?1",
            [&live.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET created_at = '2026-09-02T00:00:01Z' WHERE id = ?1",
            [&cancelled.id],
        )
        .unwrap();
    }

    let result = db.reconcile_product_executions(&product.id).unwrap();

    assert!(result.created.is_empty(), "a live execution already owns the work item");
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert_eq!(executions.len(), 2);
    assert!(executions.iter().any(|execution| execution.id == live.id));
}

#[test]
fn release_stale_claimed_executions_reverts_leftover_claimed_rows() {
    let db = WorkDb::open(temp_db_path("stale-claimed")).unwrap();
    let (product_id, first, _) = two_incomplete_project_tasks(&db);
    db.reconcile_product_executions(&product_id).unwrap();
    let execution_id = db.list_executions(Some(&first.id)).unwrap()[0].id.clone();
    db.claim_execution_for_dispatch(&execution_id).unwrap();

    let released = db.release_stale_claimed_executions().unwrap();
    assert_eq!(released, vec![execution_id.clone()]);
    assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    assert!(db.release_stale_claimed_executions().unwrap().is_empty());
}
