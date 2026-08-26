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
