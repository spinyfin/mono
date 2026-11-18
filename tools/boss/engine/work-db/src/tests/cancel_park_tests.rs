//! Cancelling a queued card's governing execution stops the card in Backlog
//! (`park_work_item_after_cancel`): automatic start off, and a halt stamp
//! that says who cancelled it, why, and how to resume. Also covers how that
//! stop sits beside parking, un-park and the stuck classification, and the
//! durable record every cancel leaves (`cancel_records`).

use super::*;
use crate::EXECUTION_CANCELLED_DISPATCH_FAILED_REASON;

fn task(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task/chore, got {other:?}"),
    }
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

/// The row's only execution is cancelled before it ever starts (mid-spawn,
/// or `bossctl executions cancel` on a queued row). The row is parked in
/// Backlog rather than left `todo` + `autostart` with nothing queued.
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
                reason: Some("cancel requested".to_owned()),
                queued_only: true,
                actor: StatusActor::Human,
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
        body.contains("cancelled by a human (cancel requested)"),
        "the actor and the caller's reason are rendered verbatim: {body}"
    );
    let runtime = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Cancelled));
    assert_eq!(
        runtime.dispatch_wait_reason, None,
        "a stopped row is not queued, so it carries no wait reason"
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
        .cancel_running_execution_and_demote_task_as(&execution.id, StatusActor::Human, "bossctl agents stop")
        .unwrap();
    assert!(cancelled && moved);
    let parked = task(&db, &chore_id);
    assert_parked_by_cancel(&parked, &execution.id);
    assert!(
        parked
            .dispatch_failed_error
            .as_deref()
            .unwrap()
            .contains("cancelled by a human (bossctl agents stop)")
    );
}

/// An engine-decided cancel names the engine in the banner.
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

/// A cancel stamp is not erased by the attention pass on the strength of the
/// cancelled execution's own run, even when it started in the same second.
#[test]
fn attention_reconcile_keeps_a_cancel_park_stamped_in_the_run_start_second() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.start_execution_run(&execution.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    db.cancel_running_execution_and_demote_task_as(&execution.id, StatusActor::Human, "stop")
        .unwrap();
    // Force the run start and the stamp into the same epoch second.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET started_at = '1700000000' WHERE execution_id = ?1",
            [&execution.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_at = '1700000000' WHERE id = ?1",
            [&chore_id],
        )
        .unwrap();

    db.reconcile_stale_attention_signals().unwrap();

    let row = task(&db, &chore_id);
    assert_eq!(
        row.dispatch_failed_reason.as_deref(),
        Some(EXECUTION_CANCELLED_DISPATCH_FAILED_REASON)
    );
    assert!(!row.autostart);
}

/// An earlier execution that ran and completed in the same second a later
/// execution was queued and cancelled must not erase the later cancel's park.
#[test]
fn attention_reconcile_keeps_a_cancel_park_when_an_earlier_execution_ran_in_the_same_second() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let earlier = create_ready_chore_execution(&db, chore_id.clone());
    db.start_execution_run(&earlier.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    db.cancel_running_execution_for_requeue(&earlier.id).unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'todo' WHERE id = ?1", [&chore_id])
        .unwrap();
    let later = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.cancel_execution_with(
        &later.id,
        CancelExecutionOpts {
            actor: StatusActor::Human,
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();
    for sql in [
        "UPDATE work_runs SET started_at = '1700000000' WHERE execution_id = ?1",
        "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
    ] {
        db.connect().unwrap().execute(sql, [&earlier.id]).unwrap();
    }
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_at = '1700000000' WHERE id = ?1",
            [&chore_id],
        )
        .unwrap();

    db.reconcile_stale_attention_signals().unwrap();

    let row = task(&db, &chore_id);
    assert_parked_by_cancel(&row, &later.id);
}

/// Only a start resumes a stopped row, and the run it starts takes the banner
/// down.
#[test]
fn starting_a_fresh_run_clears_the_stop() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.cancel_execution(&execution.id).unwrap();
    assert_parked_by_cancel(&task(&db, &chore_id), &execution.id);

    let resumed = create_ready_chore_execution(&db, chore_id.clone());
    db.start_execution_run(&resumed.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();

    let running = task(&db, &chore_id);
    assert_eq!(running.status, TaskStatus::Active);
    assert_eq!(running.dispatch_failed_reason, None);
    assert_eq!(running.dispatch_failed_error, None);
}

/// The stop is written through the task status ledger, so `boss task show`
/// can say who switched automatic start off and on what basis.
#[test]
fn the_stop_is_attributed_in_the_task_status_ledger() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.cancel_execution_with(
        &execution.id,
        CancelExecutionOpts {
            actor: StatusActor::Boss,
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();

    let (basis, actor): (String, String) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT basis, actor FROM task_status_audit
             WHERE work_item_id = ?1 AND property = 'autostart' ORDER BY id DESC LIMIT 1",
            [&chore_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(basis, crate::BASIS_STOPPED_AFTER_CANCEL);
    assert_eq!(actor, "boss");
}

/// Parking cancels a row's never-started executions itself. That cancel must
/// not stop the row: un-parking mints the execution afresh, which a row with
/// automatic start off would never get.
#[test]
fn a_park_cancel_does_not_stop_the_row_and_unpark_requeues_it() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.park_work_item_as_actor(&chore_id, true, Some("not now"), boss_protocol::LAST_STATUS_ACTOR_HUMAN)
        .unwrap();
    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Cancelled
    );
    let parked = task(&db, &chore_id);
    assert!(parked.autostart, "the park's own cancel leaves automatic start on");
    assert_eq!(parked.dispatch_failed_reason, None);
    let record = cancel_record(&db, &execution.id);
    assert_eq!(record.actor, "human", "the park's own actor, not the engine");
    assert_eq!(record.reason, "parked: not now");

    db.park_work_item_as_actor(&chore_id, false, None, boss_protocol::LAST_STATUS_ACTOR_HUMAN)
        .unwrap();
    db.reconcile_item_execution(&chore_id).unwrap();
    let requeued = query_latest_execution_for_work_item(&db.connect().unwrap(), &chore_id)
        .unwrap()
        .unwrap();
    assert_ne!(requeued.id, execution.id);
    assert!(!requeued.status.is_terminal(), "{:?}", requeued.status);
}

/// A row someone stopped and then parked is still stopped after the un-park:
/// automatic start is off, so its banner has to keep saying why.
#[test]
fn unpark_keeps_the_stop_of_a_row_that_was_cancelled_before_the_park() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.cancel_execution(&execution.id).unwrap();
    assert_parked_by_cancel(&task(&db, &chore_id), &execution.id);
    // Make the stop strictly older than the park.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_at = '1700000000' WHERE id = ?1",
            [&chore_id],
        )
        .unwrap();

    db.park_work_item_as_actor(&chore_id, true, Some("not now"), boss_protocol::LAST_STATUS_ACTOR_HUMAN)
        .unwrap();
    db.park_work_item_as_actor(&chore_id, false, None, boss_protocol::LAST_STATUS_ACTOR_HUMAN)
        .unwrap();

    assert_parked_by_cancel(&task(&db, &chore_id), &execution.id);
}

/// A stopped row is a recorded decision, not a dispatch failure: the stuck
/// report files it under the manual-start hold and the progression candidate
/// views give it no stuck kind, so nothing restarts it on its own.
#[test]
fn a_stopped_row_is_a_manual_start_hold_not_a_dispatch_failure() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.cancel_execution(&execution.id).unwrap();
    let conn = db.connect().unwrap();
    let stuck_kind = |conn: &Connection| -> Option<String> {
        conn.query_row(
            "SELECT stuck_kind FROM progression_candidate_base WHERE work_item_id = ?1",
            [&chore_id],
            |row| row.get(0),
        )
        .unwrap()
    };

    let stopped = query_task(&conn, &chore_id).unwrap().unwrap();
    assert_eq!(
        crate::stuck_report::classification(&conn, &stopped, None).unwrap(),
        "S21_manual_start"
    );
    assert_eq!(stuck_kind(&conn), None);

    // The same row under any other halt reason is a dispatch failure.
    conn.execute(
        "UPDATE tasks SET dispatch_failed_reason = 'cube_workspace_lease_failed' WHERE id = ?1",
        [&chore_id],
    )
    .unwrap();
    let failed = query_task(&conn, &chore_id).unwrap().unwrap();
    assert_eq!(
        crate::stuck_report::classification(&conn, &failed, None).unwrap(),
        "S1_dispatch_failure:cube_workspace_lease_failed"
    );
    assert_eq!(stuck_kind(&conn).as_deref(), Some("S1_dispatch_failure"));
}

fn cancel_record(db: &WorkDb, execution_id: &str) -> crate::ExecutionCancelRecord {
    let records: Vec<_> = db
        .pending_cancel_records()
        .unwrap()
        .into_iter()
        .filter(|record| record.execution_id == execution_id)
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    records.into_iter().next().unwrap()
}

#[test]
fn a_cancel_records_the_named_actor_reason_and_board_effect() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.cancel_execution_with(
        &execution.id,
        CancelExecutionOpts {
            reason: Some("coordinator cancelled it".to_owned()),
            actor: StatusActor::Boss,
            queued_only: true,
            peer_pid: Some(4242),
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();

    let record = cancel_record(&db, &execution.id);
    assert_eq!(record.work_item_id, chore_id);
    assert_eq!(record.actor, "boss");
    assert_eq!(record.reason, "coordinator cancelled it");
    assert_eq!(record.from_status, "ready");
    assert!(record.work_item_moved);
    assert!(record.queued_only);
    assert_eq!(record.peer_pid, Some(4242));

    db.acknowledge_cancel_record(&execution.id).unwrap();
    assert!(db.pending_cancel_records().unwrap().is_empty());
}

/// A running-execution teardown stages the same durable record as every other
/// cancel, with the actor and reason its caller named.
#[test]
fn a_running_execution_teardown_stages_a_cancel_record() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id);
    db.start_execution_run(&execution.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();

    db.cancel_running_execution_and_demote_task_as(&execution.id, StatusActor::Human, "explicit stop")
        .unwrap();

    let record = cancel_record(&db, &execution.id);
    assert_eq!(record.actor, "human");
    assert_eq!(record.reason, "explicit stop");
    assert_eq!(record.from_status, "running");
    assert!(record.work_item_moved);
}

#[test]
fn a_dependency_block_records_the_engine_cancel() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.start_execution_run(&execution.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    let prerequisite = create_test_chore_manual(&db, product_id, "Prerequisite");

    let (_edge, cancelled) = db
        .add_dependency_with_worker_reconcile(AddDependencyInput {
            dependent: chore_id,
            prerequisite: prerequisite.id,
            relation: None,
        })
        .unwrap();
    assert_eq!(cancelled.map(|e| e.id), Some(execution.id.clone()));

    let record = cancel_record(&db, &execution.id);
    assert_eq!(record.actor, "engine");
    assert_eq!(record.reason, "dependent transitioned to blocked");
    assert_eq!(record.from_status, "running");
}

#[test]
fn a_remediation_tombstone_records_the_engine_cancel() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.test_tombstone_orphaned_remediation(&chore_id).unwrap();

    let record = cancel_record(&db, &execution.id);
    assert_eq!(record.actor, "engine");
    assert_eq!(record.reason, "orphaned review remediation tombstoned");
    assert_eq!(record.from_status, "ready");
}

/// The record commits with the cancel, and announces itself only then.
#[tokio::test]
async fn a_committed_cancel_notifies_the_publisher() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionCancelled,
    ));

    db.cancel_execution(&execution.id).unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
        .await
        .expect("a cancel is announced after commit")
        .expect("bus open");
    assert_eq!(
        event,
        boss_event_bus::Event::ExecutionCancelled {
            execution_id: execution.id,
            task_id: chore_id,
        }
    );
}

#[test]
fn a_rolled_back_cancel_leaves_no_record() {
    let (db, _, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id);
    {
        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();
        let mut pending = PendingEvents::new();
        crate::executions_runs::cancel_execution_in_tx(
            &tx,
            &mut pending,
            &execution.id,
            CancelExecutionOpts::default(),
        )
        .unwrap();
        // Dropping the transaction must discard both state and provenance.
    }
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
    assert!(db.pending_cancel_records().unwrap().is_empty());
}

/// A park the coordinator issues is recorded as the coordinator's cancel, not
/// the human's or the engine's.
#[test]
fn a_park_cancel_by_the_coordinator_is_recorded_as_boss() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.park_work_item_as_actor(&chore_id, true, None, boss_protocol::LAST_STATUS_ACTOR_BOSS)
        .unwrap();

    let record = cancel_record(&db, &execution.id);
    assert_eq!(record.actor, "boss");
    assert_eq!(record.reason, "parked");
}
