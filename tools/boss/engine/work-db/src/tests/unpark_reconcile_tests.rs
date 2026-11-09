use super::*;
use crate::parking_reconcile::{BASIS_UNPARK_CLEARED_HALT_STAMP, HaltStamp, KeptHold, UnparkReport};
use boss_protocol::{CreateProjectInput, CreateTaskInput, OperatorQuestion};

// The database half of the un-park reconciliation sequence
// (`parking_reconcile`): step 2's halt-stamp handling inside the un-park
// transaction, and the narrow seams steps 5 and 7 read and write. The steps
// that call GitHub or the dispatcher are tested in the engine core crate.

fn open() -> WorkDb {
    WorkDb::open(temp_db_path("unpark-reconcile")).unwrap()
}

fn park(db: &WorkDb, id: &str) {
    db.park_work_item_as_actor(id, true, Some("not now"), "human").unwrap();
}

fn unpark(db: &WorkDb, id: &str) -> UnparkReport {
    db.park_work_item_reporting(id, false, None, "human").unwrap().1
}

fn task(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task or chore, got {other:?}"),
    }
}

fn sql(db: &WorkDb, statement: &str, id: &str) {
    db.connect().unwrap().execute(statement, [id]).unwrap();
}

/// Stamp `reason` on the row as a dispatch-failure producer would have, at
/// `at` epoch seconds.
fn stamp_halt(db: &WorkDb, id: &str, reason: &str, at: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_reason = ?2, dispatch_failed_error = 'boom', dispatch_failed_at = ?3
             WHERE id = ?1",
            rusqlite::params![id, reason, at],
        )
        .unwrap();
}

fn project(db: &WorkDb, product_id: &str, name: &str) -> String {
    db.create_project(
        CreateProjectInput::builder()
            .product_id(product_id)
            .name(name)
            .no_design_task(true)
            .build(),
    )
    .unwrap()
    .id
}

fn project_task(db: &WorkDb, product_id: &str, project_id: &str, name: &str) -> String {
    db.create_task(
        CreateTaskInput::builder()
            .product_id(product_id)
            .project_id(project_id)
            .name(name)
            .autostart(false)
            .build(),
    )
    .unwrap()
    .id
}

fn execution_status(db: &WorkDb, execution_id: &str) -> String {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status FROM work_executions WHERE id = ?1",
            [execution_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn ledger_bases(db: &WorkDb, id: &str, property: &str) -> Vec<String> {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare("SELECT basis FROM task_status_audit WHERE work_item_id = ?1 AND property = ?2 ORDER BY id")
        .unwrap();
    stmt.query_map(rusqlite::params![id, property], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// ── step 2: halt stamps ────────────────────────────────────────────────────

#[test]
fn unpark_clears_a_halt_stamp_that_predates_the_park() {
    let db = open();
    let product = create_test_product_named(&db, "halt-cleared").id;
    let chore = create_test_chore_manual(&db, &product, "churned").id;
    stamp_halt(&db, &chore, "churn_guard", "1");
    park(&db, &chore);

    let report = unpark(&db, &chore);

    assert_eq!(report.project_id, None);
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].id, chore);
    assert_eq!(
        report.rows[0].halt_stamp,
        HaltStamp::Cleared {
            reason: "churn_guard".into()
        }
    );
    let row = task(&db, &chore);
    assert_eq!(
        (
            &row.dispatch_failed_reason,
            &row.dispatch_failed_error,
            &row.dispatch_failed_at
        ),
        (&None, &None, &None),
        "the stale stamp says nothing about now"
    );
    assert!(row.parked_at.is_none());
    assert_eq!(
        ledger_bases(&db, &chore, "dispatch_failed_reason")
            .last()
            .map(String::as_str),
        Some(BASIS_UNPARK_CLEARED_HALT_STAMP),
        "clearing the stamp is a ledger write with its own basis",
    );
}

#[test]
fn unpark_keeps_a_halt_stamp_written_while_the_row_was_parked() {
    let db = open();
    let product = create_test_product_named(&db, "halt-newer").id;
    let chore = create_test_chore_manual(&db, &product, "failed while parked").id;
    park(&db, &chore);
    // A run that was in flight when the row was parked, and failed afterwards.
    let parked_at: i64 = task(&db, &chore).parked_at.unwrap().parse().unwrap();
    stamp_halt(&db, &chore, "spawn_failed", &(parked_at + 30).to_string());

    let report = unpark(&db, &chore);

    assert!(matches!(
        &report.rows[0].halt_stamp,
        HaltStamp::Kept { reason, .. } if reason == "spawn_failed"
    ));
    assert_eq!(
        task(&db, &chore).dispatch_failed_reason.as_deref(),
        Some("spawn_failed"),
        "newer evidence is current, so it is not erased"
    );
}

#[test]
fn unpark_keeps_a_deliberate_park_stamp() {
    let db = open();
    let product = create_test_product_named(&db, "halt-deliberate").id;
    let chore = create_test_chore_manual(&db, &product, "worker declared a blocker").id;
    stamp_halt(&db, &chore, DELIBERATE_PARK_DISPATCH_FAILED_REASON, "1");
    park(&db, &chore);

    let report = unpark(&db, &chore);

    assert!(matches!(
        &report.rows[0].halt_stamp,
        HaltStamp::Kept { reason, .. } if reason == DELIBERATE_PARK_DISPATCH_FAILED_REASON
    ));
    assert_eq!(
        task(&db, &chore).dispatch_failed_reason.as_deref(),
        Some(DELIBERATE_PARK_DISPATCH_FAILED_REASON),
        "a worker-declared blocker is a question nobody has answered yet"
    );
}

#[test]
fn unpark_of_a_row_with_no_halt_stamp_reports_nothing_to_clear() {
    let db = open();
    let product = create_test_product_named(&db, "halt-absent").id;
    let chore = create_test_chore_manual(&db, &product, "clean").id;
    park(&db, &chore);

    let report = unpark(&db, &chore);

    assert_eq!(report.rows[0].halt_stamp, HaltStamp::Absent);
    assert_eq!(report.rows[0].kept_hold, None);
}

// ── step 2: holds ──────────────────────────────────────────────────────────

fn failed_with(db: &WorkDb, label: &str, basis: WorkerFailureBasis) -> String {
    let (_product, chore, execution) = make_waiting_human_chore(db, label);
    db.record_worker_failure_with_basis(&execution, "blocked", basis)
        .unwrap();
    assert_eq!(task(db, &chore).blocked_reason.as_deref(), Some("worker_failed"));
    chore
}

#[test]
fn unpark_keeps_an_operator_refusal_as_a_human_hold() {
    let db = open();
    let chore = failed_with(&db, "refused", WorkerFailureBasis::OperatorNo);
    park(&db, &chore);

    let report = unpark(&db, &chore);

    assert_eq!(report.rows[0].kept_hold, Some(KeptHold::OperatorRefusal));
    assert!(report.rows[0].kept_hold.as_ref().unwrap().is_human_hold());
    let row = task(&db, &chore);
    assert_eq!(row.status, TaskStatus::Blocked);
    assert_eq!(row.blocked_reason.as_deref(), Some("worker_failed"));
}

#[test]
fn unpark_keeps_every_other_worker_failure_as_evidence_for_progression() {
    for basis in [
        WorkerFailureBasis::DeclaredBlocked,
        WorkerFailureBasis::NudgeBreaker,
        WorkerFailureBasis::DeliveredWithoutPr,
        WorkerFailureBasis::ForeignPrBinding,
    ] {
        let db = open();
        let chore = failed_with(&db, basis.as_str(), basis);
        park(&db, &chore);

        let report = unpark(&db, &chore);

        let hold = report.rows[0].kept_hold.clone().expect("the failure is kept");
        assert_eq!(
            hold,
            KeptHold::FailureEvidence {
                basis: Some(basis.as_str().to_owned())
            }
        );
        assert!(!hold.is_human_hold(), "{basis:?} is not a recorded human decision");
        assert_eq!(hold.blocked_reason(), "worker_failed");
        let row = task(&db, &chore);
        assert_eq!(
            (row.status, row.blocked_reason.as_deref()),
            (TaskStatus::Blocked, Some("worker_failed")),
            "the failure evidence is retained until progression clears it ({basis:?})",
        );
    }
}

#[test]
fn unpark_keeps_an_open_operator_question() {
    let db = open();
    let (_product, chore, execution) = make_waiting_human_chore(&db, "asked");
    let question = OperatorQuestion {
        text: "Change the timeout?".into(),
        explanation: "The test needs a longer timeout.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    };
    crate::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &question).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "blocked", &question)
        .unwrap();
    assert_eq!(
        task(&db, &chore).blocked_reason.as_deref(),
        Some("awaiting_operator_answer")
    );
    park(&db, &chore);

    let report = unpark(&db, &chore);

    assert_eq!(report.rows[0].kept_hold, Some(KeptHold::OperatorQuestion));
    assert_eq!(
        task(&db, &chore).blocked_reason.as_deref(),
        Some("awaiting_operator_answer"),
        "a question already in front of the owner is not answered by an un-park",
    );
}

// ── the report ─────────────────────────────────────────────────────────────

#[test]
fn unparking_a_row_that_is_not_parked_reports_nothing() {
    let db = open();
    let product = create_test_product_named(&db, "idempotent").id;
    let chore = create_test_chore_manual(&db, &product, "never parked").id;
    stamp_halt(&db, &chore, "churn_guard", "1");

    let report = unpark(&db, &chore);

    assert_eq!(report, UnparkReport::default());
    assert_eq!(
        task(&db, &chore).dispatch_failed_reason.as_deref(),
        Some("churn_guard"),
        "with no park to undo, step 2 has nothing to say about the stamp"
    );
}

#[test]
fn a_park_reports_no_released_rows() {
    let db = open();
    let product = create_test_product_named(&db, "park-report").id;
    let chore = create_test_chore_manual(&db, &product, "parked").id;
    let (_item, report) = db.park_work_item_reporting(&chore, true, None, "human").unwrap();
    assert_eq!(report, UnparkReport::default());
}

#[test]
fn project_unpark_reports_each_cascaded_child_and_leaves_direct_parks_parked() {
    let db = open();
    let product = create_test_product_named(&db, "project-report").id;
    let project = project(&db, &product, "wave");
    let first = project_task(&db, &product, &project, "first");
    let second = project_task(&db, &product, &project, "second");
    let direct = project_task(&db, &product, &project, "parked on its own");
    stamp_halt(&db, &first, "churn_guard", "1");
    park(&db, &direct);
    db.park_work_item_as_actor(&project, true, None, "human").unwrap();

    let (_item, report) = db.park_work_item_reporting(&project, false, None, "human").unwrap();

    assert_eq!(report.project_id.as_deref(), Some(project.as_str()));
    let released: Vec<&str> = report.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        released.len(),
        2,
        "only rows the project's park cascaded onto: {released:?}"
    );
    assert!(released.contains(&first.as_str()) && released.contains(&second.as_str()));
    let first_row = report.rows.iter().find(|row| row.id == first).unwrap();
    assert_eq!(
        first_row.halt_stamp,
        HaltStamp::Cleared {
            reason: "churn_guard".into()
        },
        "step 2 runs for every released child, not just the project"
    );
    assert!(task(&db, &direct).parked_at.is_some(), "the owner's own park stands");
    assert!(task(&db, &first).parked_at.is_none());
}

#[test]
fn unpark_reports_a_revision_that_inherited_the_parents_stamp_with_its_parent() {
    let db = open();
    let product = create_test_product_named(&db, "revision-report").id;
    let parent = create_test_chore_manual(&db, &product, "parent").id;
    let revision = "task_rev_report".to_owned();
    {
        let conn = db.connect().unwrap();
        crate::task_status_audit::execute(
            &conn,
            "insert_revision_in_tx",
            "human",
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'rev', '', 'todo', '1', '1', ?3)",
            rusqlite::params![revision, product, parent],
        )
        .unwrap();
    }
    park(&db, &parent);
    assert!(
        task(&db, &revision).parked_at.is_some(),
        "the revision inherits the park"
    );

    let report = unpark(&db, &parent);

    let ids: Vec<&str> = report.rows.iter().map(|row| row.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![parent.as_str(), revision.as_str()],
        "parents come before revisions"
    );
    assert!(!report.rows[0].is_revision());
    assert!(report.rows[1].is_revision());
    assert_eq!(report.rows[1].parent_task_id.as_deref(), Some(parent.as_str()));
    assert!(task(&db, &revision).parked_at.is_none());
}

// ── seams for steps 5 and 7 ────────────────────────────────────────────────

#[test]
fn last_unparked_at_reads_the_latest_unpark_from_the_ledger() {
    let db = open();
    let product = create_test_product_named(&db, "last-unparked").id;
    let chore = create_test_chore_manual(&db, &product, "cycled").id;
    assert_eq!(db.last_unparked_at(&chore).unwrap(), None, "never parked");

    park(&db, &chore);
    assert_eq!(db.last_unparked_at(&chore).unwrap(), None, "parked is not un-parked");

    let before = boss_engine_utils::epoch_time::now_epoch_secs();
    unpark(&db, &chore);
    let first = db.last_unparked_at(&chore).unwrap().expect("un-parked once");
    assert!(first >= before - 1);

    park(&db, &chore);
    // The ledger stamps whole seconds; make the second un-park observably later.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE task_status_audit SET changed_at = '1' WHERE work_item_id = ?1 AND property = 'parked_at' AND new_value IS NULL",
            [&chore],
        )
        .unwrap();
    assert_eq!(db.last_unparked_at(&chore).unwrap(), Some(1));
    unpark(&db, &chore);
    assert!(
        db.last_unparked_at(&chore).unwrap().unwrap() >= first,
        "the latest un-park wins"
    );
}

#[test]
fn last_unparked_at_covers_children_released_by_a_project_unpark() {
    let db = open();
    let product = create_test_product_named(&db, "last-unparked-project").id;
    let project = project(&db, &product, "wave");
    let child = project_task(&db, &product, &project, "child");
    db.park_work_item_as_actor(&project, true, None, "human").unwrap();
    assert_eq!(db.last_unparked_at(&child).unwrap(), None);

    db.park_work_item_as_actor(&project, false, None, "human").unwrap();

    assert!(db.last_unparked_at(&child).unwrap().is_some());
}

#[test]
fn progression_grace_stamp_marks_an_open_row_and_ignores_a_finished_one() {
    let db = open();
    let product = create_test_product_named(&db, "grace").id;
    let open_row = create_test_chore_manual(&db, &product, "open").id;
    let finished = create_test_chore_manual(&db, &product, "finished").id;
    sql(&db, "UPDATE tasks SET status = 'done' WHERE id = ?1", &finished);
    assert_eq!(db.progression_grace_waived_at(&open_row).unwrap(), None);

    assert!(db.stamp_progression_grace_waived(&open_row).unwrap());
    assert!(
        !db.stamp_progression_grace_waived(&finished).unwrap(),
        "nothing left to flag"
    );
    assert!(!db.stamp_progression_grace_waived("task_missing").unwrap());

    let stamped = db.progression_grace_waived_at(&open_row).unwrap().expect("stamped");
    assert!(stamped > 0);
    assert_eq!(db.progression_grace_waived_at(&finished).unwrap(), None);
    // Idempotent: a second call refreshes the stamp and still reports success.
    assert!(db.stamp_progression_grace_waived(&open_row).unwrap());
}

#[test]
fn progression_grace_stamp_does_not_count_as_a_status_change() {
    let db = open();
    let product = create_test_product_named(&db, "grace-quiet").id;
    let chore = create_test_chore_manual(&db, &product, "quiet").id;
    let before = task(&db, &chore).updated_at;
    let audit_rows = |db: &WorkDb| -> i64 {
        db.connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1",
                [&chore],
                |r| r.get(0),
            )
            .unwrap()
    };
    let ledger_before = audit_rows(&db);

    db.stamp_progression_grace_waived(&chore).unwrap();

    assert_eq!(
        task(&db, &chore).updated_at,
        before,
        "bookkeeping for a consumer, not a state change"
    );
    assert_eq!(audit_rows(&db), ledger_before);
}

#[test]
fn reconcile_item_execution_mints_the_execution_a_park_cancelled() {
    let db = open();
    let product = create_test_product_named(&db, "remint").id;
    let chore = create_test_chore(&db, &product, "autostarted").id;
    let first = db.reconcile_item_execution(&chore).unwrap();
    assert_eq!(
        first.created.len(),
        1,
        "a fresh row is minted a ready execution: {first:?}"
    );

    park(&db, &chore);
    let parked = db.list_executions(Some(&chore)).unwrap();
    assert!(
        parked.iter().all(|execution| execution.status.is_terminal()),
        "park cancels pending executions: {parked:?}"
    );

    // While parked, the reconcile mints nothing.
    let result = db.reconcile_item_execution(&chore).unwrap();
    assert!(result.created.is_empty() && result.updated.is_empty(), "{result:?}");

    unpark(&db, &chore);
    let result = db.reconcile_item_execution(&chore).unwrap();

    assert_eq!(result.created.len(), 1, "{result:?}");
    assert_eq!(result.created[0].work_item_id, chore);
    assert_eq!(result.created[0].status, ExecutionStatus::Ready);
    let again = db.reconcile_item_execution(&chore).unwrap();
    assert!(
        again.created.is_empty(),
        "idempotent: the row already has its execution"
    );
}

#[test]
fn explicit_cancels_adjacent_to_parking_stay_cancelled() {
    let db = open();
    let product = create_test_product_named(&db, "cancel-provenance").id;
    for before_park in [true, false] {
        let name = if before_park {
            "cancel before park"
        } else {
            "cancel after unpark"
        };
        let chore = create_test_chore(&db, &product, name).id;
        let first = db.reconcile_item_execution(&chore).unwrap().created[0].clone();
        let cancelled = if before_park {
            db.cancel_execution(&first.id).unwrap();
            park(&db, &chore);
            unpark(&db, &chore);
            first
        } else {
            park(&db, &chore);
            unpark(&db, &chore);
            let next = db.reconcile_item_execution(&chore).unwrap().created[0].clone();
            db.cancel_execution(&next.id).unwrap();
            next
        };
        // Force the explicit cancellation into the same second as the park
        // or un-park: clock proximity never authorizes a fresh execution.
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET finished_at = (
                SELECT MAX(changed_at) FROM task_status_audit
                WHERE work_item_id = ?2 AND property = 'parked_at'
            ) WHERE id = ?1",
                rusqlite::params![cancelled.id, chore],
            )
            .unwrap();
        let result = db.reconcile_item_execution(&chore).unwrap();
        assert!(result.created.is_empty() && result.updated.is_empty(), "{result:?}");
        assert_eq!(execution_status(&db, &cancelled.id), "cancelled");
    }
}

#[test]
fn review_admission_refuses_a_parked_item_in_the_database() {
    let db = open();
    let product = create_test_product_named(&db, "parked-review").id;
    let chore = create_test_chore_manual(&db, &product, "review").id;
    sql(
        &db,
        "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/foo/bar/pull/1' WHERE id = ?1",
        &chore,
    );
    park(&db, &chore);
    let error = db
        .request_pr_review(&chore, &StaticPrStateChecker(PrOpenState::Open))
        .unwrap_err();
    assert!(error.to_string().contains("parked"), "{error}");
    assert!(db.list_executions(Some(&chore)).unwrap().is_empty());
}

#[test]
fn an_active_row_is_left_to_the_orphan_sweep() {
    let db = open();
    let product = create_test_product_named(&db, "cancel-active").id;
    let chore = create_active_chore(&db, &product, "active");
    let execution = create_ready_chore_execution(&db, chore.clone());
    park(&db, &chore);
    assert_eq!(execution_status(&db, &execution.id), "cancelled");
    unpark(&db, &chore);

    let result = db.reconcile_item_execution(&chore).unwrap();

    assert!(
        result.created.is_empty(),
        "the churn guard lives in the orphan sweep, so reconcile must not mint for an active row: {result:?}"
    );
}

#[test]
fn reconcile_item_execution_never_mints_for_a_worker_failed_hold() {
    let db = open();
    let chore = failed_with(&db, "held", WorkerFailureBasis::DeclaredBlocked);
    sql(&db, "UPDATE tasks SET autostart = 1 WHERE id = ?1", &chore);

    let result = db.reconcile_item_execution(&chore).unwrap();

    assert!(result.created.is_empty(), "{result:?}");
    assert_eq!(task(&db, &chore).blocked_reason.as_deref(), Some("worker_failed"));
}

#[test]
fn open_revision_ids_lists_only_open_revisions_of_the_root() {
    let db = open();
    let product = create_test_product_named(&db, "open-revisions").id;
    let parent = create_test_chore_manual(&db, &product, "parent").id;
    let other = create_test_chore_manual(&db, &product, "other parent").id;
    let insert = |id: &str, parent: &str, status: &str| {
        let conn = db.connect().unwrap();
        crate::task_status_audit::execute(
            &conn,
            "insert_revision_in_tx",
            "human",
            "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, parent_task_id)
             VALUES (?1, ?2, 'revision', 'rev', '', ?4, '1', '1', ?3)",
            rusqlite::params![id, product, parent, status],
        )
        .unwrap();
    };
    insert("task_rev_open", &parent, "todo");
    insert("task_rev_done", &parent, "done");
    insert("task_rev_other", &other, "todo");

    assert_eq!(db.open_revision_ids(&parent).unwrap(), vec!["task_rev_open".to_owned()]);
}

#[test]
fn consecutive_unproductive_streak_stops_at_the_floor() {
    let db = open();
    let product = create_test_product_named(&db, "streak").id;
    let chore = create_test_chore_manual(&db, &product, "churning").id;
    let mut ids = Vec::new();
    for created_at in [100, 200, 300] {
        let execution = create_ready_chore_execution(&db, chore.clone());
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'orphaned', created_at = ?2 WHERE id = ?1",
                rusqlite::params![execution.id, created_at.to_string()],
            )
            .unwrap();
        ids.push(execution.id);
    }

    let all = db.list_consecutive_unproductive_terminal_execution_ids(&chore).unwrap();
    assert_eq!(all.len(), 3, "the whole history without a floor");
    assert_eq!(
        db.list_consecutive_unproductive_terminal_execution_ids_since(&chore, None)
            .unwrap(),
        all
    );

    let since = db
        .list_consecutive_unproductive_terminal_execution_ids_since(&chore, Some(250))
        .unwrap();
    assert_eq!(
        since,
        vec![ids[2].clone()],
        "executions before the un-park are not strikes"
    );

    let none = db
        .list_consecutive_unproductive_terminal_execution_ids_since(&chore, Some(1_000))
        .unwrap();
    assert!(none.is_empty());
}

// A CI fix that was running when the item was parked finishes normally: its
// green report returns the parent to `in_review` through the shared clear.
// Only the un-park re-arm refuses a parked item.
#[test]
fn a_parked_item_still_returns_to_in_review_when_its_running_ci_fix_goes_green() {
    let db = open();
    let product = create_test_product_named(&db, "ci-fix-green").id;
    let chore = create_test_chore_manual(&db, &product, "ci fix running").id;
    let pr = "https://github.com/foo/bar/pull/9100";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore, pr],
        )
        .unwrap();
    park(&db, &chore);

    let cleared = db.clear_chore_blocked_ci_failure(&chore, pr).unwrap();

    assert!(cleared.is_some(), "the in-flight fix's completion proceeds");
    assert_eq!(task(&db, &chore).status, boss_protocol::TaskStatus::InReview);
}

#[test]
fn the_rearm_refuses_a_parked_item_and_touches_only_the_exhausted_reason() {
    let db = open();
    let product = create_test_product_named(&db, "rearm").id;
    let chore = create_test_chore_manual(&db, &product, "exhausted").id;
    let pr = "https://github.com/foo/bar/pull/9101";
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure_exhausted', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore, pr],
        )
        .unwrap();
    park(&db, &chore);
    assert!(db.rearm_exhausted_ci_block_unless_parked(&chore, pr).unwrap().is_none());
    assert_eq!(
        task(&db, &chore).blocked_reason.as_deref(),
        Some("ci_failure_exhausted")
    );

    unpark(&db, &chore);
    assert!(db.rearm_exhausted_ci_block_unless_parked(&chore, pr).unwrap().is_some());
    assert_eq!(task(&db, &chore).status, boss_protocol::TaskStatus::InReview);

    // A live (not exhausted) CI block is left to its fix.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure' WHERE id = ?1",
            [&chore],
        )
        .unwrap();
    assert!(db.rearm_exhausted_ci_block_unless_parked(&chore, pr).unwrap().is_none());
}
