use super::*;
use boss_engine_recovery::execution_bookmark::PreservedTip;

/// A finished predecessor that still records the cube lease it ran under, and
/// the `ready` successor of the same work item.
fn finished_predecessor_and_successor(db: &WorkDb, label: &str) -> (WorkExecution, WorkExecution) {
    let (_, task, id) = make_waiting_human_chore(db, label);
    db.record_worker_failure(&id, "worker stopped").unwrap();
    let prior = db.get_execution(&id).unwrap();
    assert!(prior.status.is_terminal());
    assert_eq!(prior.cube_lease_id.as_deref(), Some("lease-1"));
    let next = db
        .request_execution(RequestExecutionInput::builder().work_item_id(task).build())
        .unwrap();
    (prior, next)
}

#[test]
fn successor_waits_while_a_finished_predecessor_still_records_its_lease() {
    let db = WorkDb::open(temp_db_path("predecessor-lease-hold")).unwrap();
    let (prior, next) = finished_predecessor_and_successor(&db, "held");

    let held = db
        .recovery_predecessor_holding_lease(&next)
        .unwrap()
        .expect("the predecessor's work may not be preserved yet");
    assert_eq!(held.id, prior.id);

    // The release clears the lease column (preservation precedes the claim);
    // that, not the lingering workspace columns, is what ends the wait.
    assert!(db.claim_execution_workspace_release(&prior.id, "lease-1").unwrap());
    assert!(db.get_execution(&prior.id).unwrap().workspace_path.is_some());
    assert!(db.recovery_predecessor_holding_lease(&next).unwrap().is_none());
}

#[test]
fn an_old_lease_still_holds_the_successor_and_is_retried_by_the_release_sweep() {
    let db = WorkDb::open(temp_db_path("predecessor-lease-window")).unwrap();
    let (prior, next) = finished_predecessor_and_successor(&db, "stale");
    let ended = boss_engine_utils::epoch_time::now_epoch_secs() - 86_400 - 60;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET finished_at = ?2 WHERE id = ?1",
            params![prior.id, ended.to_string()],
        )
        .unwrap();

    assert!(
        db.list_terminal_executions_holding_lease()
            .unwrap()
            .iter()
            .any(|e| e.id == prior.id),
        "the sweep must still preserve and release this lease"
    );
    assert!(db.recovery_predecessor_holding_lease(&next).unwrap().is_some());
}

#[test]
fn a_successor_that_recovers_from_its_own_preserved_tip_does_not_wait() {
    let db = WorkDb::open(temp_db_path("predecessor-lease-own-tip")).unwrap();
    let (_, next) = finished_predecessor_and_successor(&db, "own-tip");
    db.record_preserved_tip(
        &next.id,
        &PreservedTip {
            change_id: "change".into(),
            commit_id: "commit".into(),
            count: 1,
        },
    )
    .unwrap();

    assert!(db.recovery_predecessor_holding_lease(&next).unwrap().is_none());
}

#[test]
fn a_predecessor_that_is_still_live_is_not_waited_for() {
    let db = WorkDb::open(temp_db_path("predecessor-lease-live")).unwrap();
    let (_, task, id) = make_waiting_human_chore(&db, "live");
    assert!(!db.get_execution(&id).unwrap().status.is_terminal());
    let next = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(task)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .repo_remote_url("git@github.com:foo/bar.git")
                .build(),
        )
        .unwrap();

    assert!(db.recovery_predecessor_holding_lease(&next).unwrap().is_none());
}

#[test]
fn a_successor_with_no_predecessor_does_not_wait() {
    let db = WorkDb::open(temp_db_path("predecessor-lease-none")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id, "First run");
    let first = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    assert!(db.recovery_predecessor_holding_lease(&first).unwrap().is_none());
}

#[test]
fn a_resume_row_names_no_workspace() {
    let db = WorkDb::open(temp_db_path("resume-row-shape")).unwrap();
    let (_, task, id) = make_waiting_human_chore(&db, "resume");
    db.force_transient_failure_count_for_test(&id, 1).unwrap();

    let resumed = db.request_resume_execution(&id, 2, 0, "test").unwrap();

    assert_eq!(resumed.work_item_id, task);
    assert_eq!(resumed.status, ExecutionStatus::Ready);
    assert_eq!(resumed.transient_failure_count, 2);
    assert_eq!(resumed.preferred_workspace_id, None);
    assert!(!resumed.allow_dirty);
    assert_eq!(db.get_execution(&id).unwrap().status, ExecutionStatus::Orphaned);
    // The stalled run keeps its lease until the caller preserves and releases
    // it, and that is what the successor waits for.
    let held = db.recovery_predecessor_holding_lease(&resumed).unwrap();
    assert_eq!(held.map(|e| e.id), Some(id));
}

#[test]
fn resume_reuses_a_successor_inserted_during_teardown() {
    let db = WorkDb::open(temp_db_path("resume-insertion-race")).unwrap();
    let (prior, next) = finished_predecessor_and_successor(&db, "concurrent resume");
    let resumed = db.request_resume_execution(&prior.id, 1, 0, "retry").unwrap();
    assert_eq!(resumed.id, next.id);
    assert_eq!(db.list_executions(Some(&prior.work_item_id)).unwrap().len(), 2);
}
