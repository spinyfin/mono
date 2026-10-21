//! A `todo` row flagged `autostart` must never present as queued while the
//! engine has nothing queued for it. Covers the cancel-time park
//! (`park_work_item_after_cancel`), the reconciler's convergence of rows
//! stranded before that park existed (`converge_stranded_autostart_row`),
//! and the engine-stated hold reason on `TaskRuntime`.

use super::*;
use crate::work::EXECUTION_CANCELLED_DISPATCH_FAILED_REASON;

fn task(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task/chore, got {other:?}"),
    }
}

/// Flip an execution terminal with a raw write, bypassing every cancel path
/// — the shape a database stranded by an older engine carries.
fn raw_terminalize(db: &WorkDb, execution_id: &str, status: &str, started: bool) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions
         SET status = ?2, finished_at = '1700000100',
             started_at = CASE WHEN ?3 THEN '1700000050' ELSE started_at END
         WHERE id = ?1",
        params![execution_id, status, started],
    )
    .unwrap();
}

fn assert_parked_by_cancel(task: &Task, execution_id: &str) {
    assert_eq!(task.status, TaskStatus::Todo);
    assert!(
        !task.autostart,
        "a cancel-parked row must leave the dispatch-pending set"
    );
    assert_eq!(
        task.dispatch_failed_reason.as_deref(),
        Some(EXECUTION_CANCELLED_DISPATCH_FAILED_REASON)
    );
    let body = task.dispatch_failed_error.as_deref().unwrap_or_default();
    assert!(body.contains(execution_id), "park text must name the execution: {body}");
    assert!(
        body.contains("bossctl work start"),
        "park text must say how to resume: {body}"
    );
    assert!(task.dispatch_failed_at.is_some());
}

/// The reported incident shape: the row's only execution is cancelled before
/// it ever starts (mid-spawn, or `bossctl executions cancel` on a queued
/// row). The row used to stay `todo` + `autostart` with nothing queued and
/// render in Doing as "Queued — reason unknown" forever.
#[test]
fn cancelling_the_governing_never_started_execution_parks_the_row() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    assert!(
        task(&db, &chore_id).autostart,
        "fixture: a fresh chore is dispatch-pending"
    );

    let outcome = db
        .cancel_execution_with_outcome(
            &execution.id,
            CancelExecutionOpts {
                reason: Some("operator cancelled it".to_owned()),
                queued_only: true,
                actor: CancelActor::Human,
                ..CancelExecutionOpts::default()
            },
        )
        .unwrap();
    assert!(
        outcome.work_item_parked,
        "the outcome reports the park so the caller can log it"
    );
    assert_eq!(outcome.execution.status, ExecutionStatus::Cancelled);

    let parked = task(&db, &chore_id);
    assert_parked_by_cancel(&parked, &execution.id);
    let body = parked.dispatch_failed_error.as_deref().unwrap();
    assert!(
        body.contains("cancelled by an operator (operator cancelled it)"),
        "the actor and the caller's reason are rendered verbatim: {body}"
    );
    let runtime = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Cancelled));
    assert_eq!(
        runtime.dispatch_hold_reason, None,
        "a parked row is not dispatch-pending, so it carries no hold reason"
    );
}

#[test]
fn cancelling_a_live_execution_parks_the_row_with_the_reason() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.start_execution_run(&execution.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    assert_eq!(task(&db, &chore_id).status, TaskStatus::Active);

    let (cancelled, moved) = db
        .cancel_running_execution_and_demote_task_as(&execution.id, CancelActor::Human, "bossctl agents stop")
        .unwrap();
    assert!(cancelled && moved);
    let parked = task(&db, &chore_id);
    assert_parked_by_cancel(&parked, &execution.id);
    assert!(
        parked
            .dispatch_failed_error
            .as_deref()
            .unwrap()
            .contains("cancelled by an operator (bossctl agents stop)")
    );
}

/// An engine-decided cancel names the engine, so the operator reading the
/// banner knows no human stopped it.
#[test]
fn an_engine_cancel_names_the_engine_in_the_park_text() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.cancel_execution_with(
        &execution.id,
        CancelExecutionOpts {
            reason: Some("requested host became ineligible: host offline".to_owned()),
            queued_only: true,
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();

    let parked = task(&db, &chore_id);
    assert_parked_by_cancel(&parked, &execution.id);
    assert!(
        parked
            .dispatch_failed_error
            .as_deref()
            .unwrap()
            .contains("cancelled by the engine (requested host became ineligible: host offline)")
    );
}

#[test]
fn opting_out_leaves_the_row_schedulable_for_the_caller_to_replace() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.cancel_execution_with(
        &execution.id,
        CancelExecutionOpts {
            reason: Some("replaced in the same transaction".to_owned()),
            keep_work_item_schedulable: true,
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();

    let untouched = task(&db, &chore_id);
    assert!(untouched.autostart);
    assert_eq!(untouched.dispatch_failed_reason, None);

    // The requeue variant used by scheduler preemption makes the same promise.
    let next = create_ready_chore_execution(&db, chore_id.clone());
    assert!(db.cancel_running_execution_for_requeue(&next.id).unwrap());
    let still = task(&db, &chore_id);
    assert!(still.autostart);
    assert_eq!(still.dispatch_failed_reason, None);
}

#[test]
fn cancelling_a_stale_duplicate_leaves_the_governing_row_alone() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let stale = create_ready_chore_execution(&db, chore_id.clone());
    // A newer execution owns the row; the stale one is shadowed.
    let _governing = create_ready_chore_execution(&db, chore_id.clone());

    db.cancel_execution(&stale.id).unwrap();

    let row = task(&db, &chore_id);
    assert!(row.autostart, "cancelling a shadowed duplicate must not park the row");
    assert_eq!(row.dispatch_failed_reason, None);
}

/// A database stranded by an older engine: the only execution is
/// `cancelled`, never started, and the row is still `todo` + `autostart`.
/// The reconciler converges it onto the halt surface instead of leaving a
/// phantom "Queued" card, and mints nothing.
#[test]
fn reconcile_parks_a_stranded_row_whose_only_execution_was_cancelled_before_start() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    raw_terminalize(&db, &execution.id, "cancelled", false);
    assert!(task(&db, &chore_id).autostart, "fixture: the stranded shape");
    assert_eq!(
        db.get_task_runtime(&chore_id).unwrap().dispatch_hold_reason.as_deref(),
        Some("awaiting_reconcile"),
        "before convergence the engine states that the reconciler owns the next step"
    );

    let result = db.reconcile_product_executions(&product_id).unwrap();

    assert!(
        result.created.is_empty(),
        "a cancel is a stop: nothing is minted behind it"
    );
    let parked = task(&db, &chore_id);
    assert_parked_by_cancel(&parked, &execution.id);
    let body = parked.dispatch_failed_error.as_deref().unwrap();
    assert!(
        body.contains("cancelled by a party the engine did not record")
            && body.contains("did not record the cancel reason"),
        "the park text is honest about what the engine knows: {body}"
    );
    assert_eq!(db.list_executions(Some(&chore_id)).unwrap().len(), 1);
    assert_eq!(db.get_task_runtime(&chore_id).unwrap().dispatch_hold_reason, None);

    // Idempotent: a second pass finds nothing dispatch-pending.
    let again = db.reconcile_product_executions(&product_id).unwrap();
    assert!(again.created.is_empty() && again.updated.is_empty());
}

/// `start_execution_run` consumes `autostart`, so `autostart = 1` behind an
/// execution that ran means the row was re-armed afterwards. The reconciler
/// honours that by minting a replacement — the row stops being "queued with
/// nothing queued" the other way round.
#[test]
fn reconcile_mints_a_replacement_for_a_rearmed_row_behind_a_started_terminal_execution() {
    for status in ["completed", "cancelled", "orphaned"] {
        let (db, product_id, chore_id) = setup_product_and_chore();
        let execution = create_ready_chore_execution(&db, chore_id.clone());
        raw_terminalize(&db, &execution.id, status, true);
        assert!(task(&db, &chore_id).autostart);

        let result = db.reconcile_product_executions(&product_id).unwrap();

        assert_eq!(
            result.created.len(),
            1,
            "{status}: a re-armed row gets a fresh execution"
        );
        assert_eq!(result.created[0].status, ExecutionStatus::Ready);
        let executions = db.list_executions(Some(&chore_id)).unwrap();
        assert_eq!(executions.len(), 2, "{status}");
        let row = task(&db, &chore_id);
        assert!(
            row.autostart,
            "{status}: minting does not consume autostart; the run start does"
        );
        assert_eq!(row.dispatch_failed_reason, None, "{status}");
        let runtime = db.get_task_runtime(&chore_id).unwrap();
        assert_eq!(runtime.execution_status, Some(ExecutionStatus::Ready), "{status}");
        assert_eq!(runtime.dispatch_hold_reason, None, "{status}");
    }
}

/// A dispatch-pending row the reconciler declines to mint for carries the
/// gate that holds it, so the card can say so instead of "reason unknown".
#[test]
fn task_runtime_states_why_a_dispatch_pending_row_has_nothing_queued() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    raw_terminalize(&db, &execution.id, "completed", true);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/example/repo/pull/7' WHERE id = ?1",
            [&chore_id],
        )
        .unwrap();

    let before = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(before.dispatch_hold_reason.as_deref(), Some("pr_awaits_review"));

    let result = db.reconcile_product_executions(&product_id).unwrap();
    assert!(result.created.is_empty(), "a bound PR is a decline-to-mint gate");
    let after = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(after.dispatch_hold_reason.as_deref(), Some("pr_awaits_review"));
    assert_eq!(after.execution_status, Some(ExecutionStatus::Completed));

    // A pending execution owns the description; no hold reason is stated.
    let pending = create_ready_chore_execution(&db, chore_id.clone());
    let queued = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(queued.execution_id.as_deref(), Some(pending.id.as_str()));
    assert_eq!(queued.dispatch_hold_reason, None);
}
