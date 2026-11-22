//! The card notice for an engine-initiated stop: which ledger rows raise it,
//! what clears it, and that every read path ships the same notice.
use super::board_projection_tests::check;
use super::*;
use crate::task_status_audit::{self, abandon_cause};
use boss_protocol::AutoStopNotice;

fn card(db: &WorkDb, id: &str) -> Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn stop_notice(db: &WorkDb, id: &str) -> Option<AutoStopNotice> {
    card(db, id).auto_stop_notice
}

fn ready_execution(db: &WorkDb, work_item_id: &str) -> WorkExecution {
    create_ready_chore_execution(db, work_item_id)
}

/// A never-started item whose pending execution the heal abandons. The chore
/// is created with autostart off, as an item the engine will not re-mint.
fn ghost_demoted(db: &WorkDb, product: &str, name: &str) -> (String, String) {
    ghost_demoted_with_autostart(db, product, name, false)
}

/// Same heal as [`ghost_demoted`], with autostart left on so the reconciler
/// is supposed to re-mint. The notice still raises until something is queued.
fn ghost_demoted_with_autostart(db: &WorkDb, product: &str, name: &str, autostart: bool) -> (String, String) {
    let chore = if autostart {
        create_test_chore(db, product, name)
    } else {
        create_test_chore_manual(db, product, name)
    };
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    let execution = ready_execution(db, &chore.id);
    assert_eq!(db.heal_ghost_active_chores().unwrap().len(), 1);
    (chore.id, execution.id)
}

/// One finished run whose timestamps sit before a later stop, so a historical
/// run cannot consume the notice by same-second comparison.
fn finish_a_run_then_backdate(db: &WorkDb, item: &str) {
    let execution = db
        .list_executions(Some(item))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| ready_execution(db, item));
    let (_, run) = db
        .start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Completed)
            .run_status("completed")
            .build(),
    )
    .unwrap();
    let past = boss_engine_utils::epoch_time::now_epoch_secs() - 10;
    db.force_started_at_for_test(&execution.id, past).unwrap();
    db.force_latest_run_started_at_for_test(&execution.id, past).unwrap();
}

/// An abandonment written without the ledger helpers, as an unknown writer would.
fn raw_abandon(db: &WorkDb, execution_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            [execution_id],
        )
        .unwrap();
}

/// An engine `autostart` 1→0 row in the pre-enrichment shape: `basis` names
/// the writer, every added column is NULL. Matches rows recorded between the
/// original ledger and migration `20261118_1530_execution_abandon_audit`.
fn insert_pre_enrichment_autostart_off(db: &WorkDb, work_item_id: &str, basis: &str) {
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO task_status_audit
                (work_item_id, property, old_value, new_value, actor, changed_at, basis,
                 reason_code, operation_id, detail, execution_id, failure_stamp, failure_error,
                 execution_ever_ran, item_ever_ran)
             VALUES (?1, 'autostart', '1', '0', 'engine', strftime('%s','now'), ?2,
                     NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL)",
            rusqlite::params![work_item_id, basis],
        )
        .unwrap();
}

fn acknowledge(db: &WorkDb, id: &str, through: i64) -> Task {
    match db
        .update_work_item_as_actor(
            id,
            WorkItemPatch {
                acknowledge_auto_stop_through: Some(through),
                ..WorkItemPatch::default()
            },
            "human",
        )
        .unwrap()
    {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

#[test]
fn an_abandoned_never_started_execution_with_no_replacement_raises_the_notice() {
    let db = WorkDb::open(temp_db_path("auto-stop-abandon")).unwrap();
    let product = create_test_product_named(&db, "Auto stop abandon");
    let (item, execution) = ghost_demoted(&db, &product.id, "Ghost");

    let notice = stop_notice(&db, &item).expect("the engine withdrew the only execution");
    assert_eq!(notice.reason_code, abandon_cause::GHOST_ACTIVE_DEMOTED.code);
    assert_eq!(
        notice.detail.as_deref(),
        Some(abandon_cause::GHOST_ACTIVE_DEMOTED.detail)
    );
    assert!(notice.execution_abandoned && !notice.autostart_disabled);
    assert!(notice.changed_at.parse::<i64>().unwrap() > 0);

    // The notice is a view of the ledger row `boss task show` lists.
    let row = db
        .list_task_field_audit(&item)
        .unwrap()
        .into_iter()
        .find(|row| row.property == task_status_audit::EXECUTION_ABANDONED_PROPERTY)
        .unwrap();
    assert_eq!(row.execution_id.as_deref(), Some(execution.as_str()));
    assert_eq!(row.reason_code.as_deref(), Some(notice.reason_code.as_str()));
    assert_eq!(
        (row.old_value.as_deref(), row.new_value.as_deref()),
        (Some("ready"), Some("abandoned"))
    );
}

#[test]
fn a_combined_abandon_and_autostart_off_is_one_notice_naming_both() {
    let db = WorkDb::open(temp_db_path("auto-stop-combined")).unwrap();
    let product = create_test_product_named(&db, "Auto stop combined");
    let chore = create_test_chore(&db, &product.id, "Armed");
    let execution = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| ready_execution(&db, &chore.id));
    assert_eq!(stop_notice(&db, &chore.id), None);

    {
        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();
        let operation = task_status_audit::begin_operation(&tx).unwrap();
        task_status_audit::abandon(
            &tx,
            abandon_cause::GHOST_ACTIVE_DEMOTED,
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
        task_status_audit::execute(
            &tx,
            "engine_gave_up_starting",
            "engine",
            "UPDATE tasks SET autostart = 0, status = 'todo' WHERE id = ?1",
            [&chore.id],
        )
        .unwrap();
        operation.end().unwrap();
        tx.commit().unwrap();
    }

    let notice = stop_notice(&db, &chore.id).expect("both writes left the item stopped");
    assert!(notice.execution_abandoned && notice.autostart_disabled);
    let rows = db.list_task_field_audit(&chore.id).unwrap();
    let abandon = rows
        .iter()
        .find(|row| row.property == task_status_audit::EXECUTION_ABANDONED_PROPERTY)
        .unwrap();
    let autostart = rows.iter().rfind(|row| row.property == "autostart").unwrap();
    assert_eq!(abandon.operation_id, autostart.operation_id);
    assert_eq!(
        (autostart.old_value.as_deref(), autostart.new_value.as_deref()),
        (Some("1"), Some("0"))
    );
    assert_eq!(autostart.reason_code.as_deref(), Some("engine_gave_up_starting"));
    // The notice names the latest row it covers, so one acknowledgment clears both.
    assert!(acknowledge(&db, &chore.id, notice.audit_id).auto_stop_notice.is_none());
}

#[test]
fn a_dispatch_failure_bounce_raises_the_notice_until_autostart_is_rearmed() {
    let db = WorkDb::open(temp_db_path("auto-stop-bounce")).unwrap();
    let product = create_test_product_named(&db, "Auto stop bounce");
    let chore = create_test_chore(&db, &product.id, "Bounced");
    assert!(
        db.bounce_dispatch_failed_to_backlog(&chore.id, "cube_workspace_lease_failed", "no workspace")
            .unwrap()
    );
    let notice = stop_notice(&db, &chore.id).unwrap();
    assert_eq!(notice.reason_code, "cube_workspace_lease_failed");
    assert!(notice.autostart_disabled && !notice.execution_abandoned);

    db.reenable_and_request_execution_with_live_check(
        &chore.id,
        RequestExecutionInput::builder().work_item_id(chore.id.clone()).build(),
        |_| true,
    )
    .unwrap();
    assert_eq!(stop_notice(&db, &chore.id), None);
}

#[test]
fn a_queued_execution_clears_an_autostart_off_notice_without_rearming() {
    let db = WorkDb::open(temp_db_path("auto-stop-queue-clears")).unwrap();
    let product = create_test_product_named(&db, "Auto stop queue");
    let chore = create_test_chore(&db, &product.id, "Queued");
    assert!(
        db.bounce_dispatch_failed_to_backlog(&chore.id, "cube_workspace_lease_failed", "no workspace")
            .unwrap()
    );
    assert!(stop_notice(&db, &chore.id).unwrap().autostart_disabled);

    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    assert!(!card(&db, &chore.id).autostart);
    assert_eq!(
        stop_notice(&db, &chore.id),
        None,
        "a queued execution means the item is moving again even with autostart still off"
    );
}

#[test]
fn an_abandoned_never_started_execution_raises_the_notice_while_autostart_is_on() {
    let db = WorkDb::open(temp_db_path("auto-stop-abandon-autostart-on")).unwrap();
    let product = create_test_product_named(&db, "Auto stop autostart on");
    let (item, _) = ghost_demoted_with_autostart(&db, &product.id, "Ghost armed", true);
    assert!(card(&db, &item).autostart);
    let notice = stop_notice(&db, &item).expect("an unreplaced abandon is a stall even with autostart on");
    assert!(notice.execution_abandoned && !notice.autostart_disabled);
}

#[test]
fn a_previous_run_does_not_hide_an_engine_autostart_off() {
    let db = WorkDb::open(temp_db_path("auto-stop-after-run-bounce")).unwrap();
    let product = create_test_product_named(&db, "Auto stop after run bounce");
    let acked = create_test_chore(&db, &product.id, "Acked");
    let started = create_test_chore(&db, &product.id, "Started");
    finish_a_run_then_backdate(&db, &acked.id);
    finish_a_run_then_backdate(&db, &started.id);

    for item in [&acked.id, &started.id] {
        db.update_work_item_as_actor(
            item,
            WorkItemPatch {
                autostart: Some(true),
                status: Some("todo".to_owned()),
                ..WorkItemPatch::default()
            },
            "human",
        )
        .unwrap();
        assert!(
            db.bounce_dispatch_failed_to_backlog(item, "cube_workspace_lease_failed", "no workspace")
                .unwrap()
        );
        let notice = stop_notice(&db, item).expect("a later autostart-off is visible after a finished run");
        assert!(notice.autostart_disabled && !notice.execution_abandoned);
    }

    let first = stop_notice(&db, &acked.id).unwrap();
    assert!(acknowledge(&db, &acked.id, first.audit_id).auto_stop_notice.is_none());

    let replacement = ready_execution(&db, &started.id);
    db.start_execution_run(&replacement.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert_eq!(stop_notice(&db, &started.id), None);
}

#[test]
fn a_previous_run_does_not_hide_an_unreplaced_abandonment() {
    let db = WorkDb::open(temp_db_path("auto-stop-after-run-abandon")).unwrap();
    let product = create_test_product_named(&db, "Auto stop after run abandon");
    let acked = create_test_chore(&db, &product.id, "Acked");
    let started = create_test_chore(&db, &product.id, "Started");
    finish_a_run_then_backdate(&db, &acked.id);
    finish_a_run_then_backdate(&db, &started.id);

    raw_abandon(&db, &ready_execution(&db, &acked.id).id);
    raw_abandon(&db, &ready_execution(&db, &started.id).id);
    let first = stop_notice(&db, &acked.id).expect("an unreplaced later attempt is visible after a finished run");
    assert!(first.execution_abandoned);
    assert!(stop_notice(&db, &started.id).is_some());

    assert!(acknowledge(&db, &acked.id, first.audit_id).auto_stop_notice.is_none());
    let replacement = ready_execution(&db, &started.id);
    db.start_execution_run(&replacement.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert_eq!(stop_notice(&db, &started.id), None);
}

#[test]
fn normal_run_start_consumption_of_autostart_is_not_a_stop() {
    let db = WorkDb::open(temp_db_path("auto-stop-run-start")).unwrap();
    let product = create_test_product_named(&db, "Auto stop run start");
    let chore = create_test_chore(&db, &product.id, "Started");
    let execution = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| ready_execution(&db, &chore.id));
    db.start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    let rows = db.list_task_field_audit(&chore.id).unwrap();
    assert!(
        rows.iter()
            .any(|row| row.property == "autostart" && row.new_value.as_deref() == Some("0")),
        "the consumption is still recorded in the ledger"
    );
    assert_eq!(stop_notice(&db, &chore.id), None);
}

#[test]
fn starting_a_run_clears_a_notice_by_itself() {
    let db = WorkDb::open(temp_db_path("auto-stop-clears-on-start")).unwrap();
    let product = create_test_product_named(&db, "Auto stop clears");
    let (item, _) = ghost_demoted(&db, &product.id, "Ghost");
    assert!(stop_notice(&db, &item).is_some());
    let replacement = ready_execution(&db, &item);
    db.start_execution_run(&replacement.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert_eq!(stop_notice(&db, &item), None);
}

#[test]
fn an_abandoned_attempt_that_was_replaced_is_not_a_stop() {
    let db = WorkDb::open(temp_db_path("auto-stop-replaced")).unwrap();
    let product = create_test_product_named(&db, "Auto stop replaced");
    let (item, _) = ghost_demoted(&db, &product.id, "Ghost");
    assert!(stop_notice(&db, &item).is_some());
    ready_execution(&db, &item);
    assert_eq!(
        stop_notice(&db, &item),
        None,
        "a queued replacement means the item is still moving"
    );
}

#[test]
fn a_withdrawal_that_is_the_intended_outcome_is_not_a_stop() {
    let db = WorkDb::open(temp_db_path("auto-stop-benign")).unwrap();
    let product = create_test_product_named(&db, "Auto stop benign");
    let chore = create_test_chore_manual(&db, &product.id, "Duplicate");
    let execution = ready_execution(&db, &chore.id);
    db.mark_execution_redundant(&execution.id).unwrap();
    assert!(
        db.list_task_field_audit(&chore.id)
            .unwrap()
            .iter()
            .any(|row| row.reason_code.as_deref() == Some(abandon_cause::REDUNDANT_DUPLICATE.code)),
        "the abandonment is recorded"
    );
    assert_eq!(stop_notice(&db, &chore.id), None);
}

#[test]
fn a_human_stop_and_a_deliberate_deferral_are_not_engine_stops() {
    let db = WorkDb::open(temp_db_path("auto-stop-human")).unwrap();
    let product = create_test_product_named(&db, "Auto stop human");
    let chore = create_test_chore(&db, &product.id, "Switched off");
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            autostart: Some(false),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    assert_eq!(stop_notice(&db, &chore.id), None);

    let deferred = create_test_chore(&db, &product.id, "Deferred");
    db.update_work_item_as_actor(
        &deferred.id,
        WorkItemPatch {
            deferred: Some(true),
            autostart: Some(false),
            ..WorkItemPatch::default()
        },
        "engine",
    )
    .unwrap();
    assert_eq!(stop_notice(&db, &deferred.id), None);
}

#[test]
fn the_notice_survives_status_changes_until_acknowledged() {
    let db = WorkDb::open(temp_db_path("auto-stop-persists")).unwrap();
    let product = create_test_product_named(&db, "Auto stop persists");
    let (item, _) = ghost_demoted(&db, &product.id, "Ghost");
    let first = stop_notice(&db, &item).unwrap();
    for status in ["blocked", "todo"] {
        db.update_work_item_as_actor(
            &item,
            WorkItemPatch {
                status: Some(status.to_owned()),
                ..WorkItemPatch::default()
            },
            "human",
        )
        .unwrap();
        assert_eq!(
            stop_notice(&db, &item).as_ref(),
            Some(&first),
            "after moving to {status}"
        );
    }
}

#[test]
fn acknowledging_never_dismisses_a_newer_or_future_stop() {
    let db = WorkDb::open(temp_db_path("auto-stop-ack")).unwrap();
    let product = create_test_product_named(&db, "Auto stop ack");
    let (item, _) = ghost_demoted(&db, &product.id, "Ghost");
    let first = stop_notice(&db, &item).unwrap();

    // An acknowledgment that names a row the item does not have yet cannot
    // pre-dismiss the next stop.
    acknowledge(&db, &item, i64::MAX);
    assert_eq!(stop_notice(&db, &item), None);
    let execution = ready_execution(&db, &item);
    raw_abandon(&db, &execution.id);
    let second = stop_notice(&db, &item).expect("a stop recorded after the acknowledgment is new");
    assert!(second.audit_id > first.audit_id);

    // A stale acknowledgment of the first stop does not dismiss the second,
    // and does not lower what was acknowledged.
    assert_eq!(
        acknowledge(&db, &item, first.audit_id).auto_stop_notice,
        Some(second.clone())
    );
    assert!(acknowledge(&db, &item, second.audit_id).auto_stop_notice.is_none());
    assert_eq!(stop_notice(&db, &item), None);
}

#[test]
fn board_reads_agree_with_a_full_rebuild_across_every_transition() {
    let db = WorkDb::open(temp_db_path("auto-stop-board")).unwrap();
    let product = create_test_product_named(&db, "Auto stop board");
    let mut model = None;
    check(&db, &product.id, &mut model, 100);

    let (item, _) = ghost_demoted(&db, &product.id, "Ghost");
    check(&db, &product.id, &mut model, 101);
    let on_board = |db: &WorkDb, now: i64, model: &mut Option<_>| {
        let (tree, _) = db
            .refresh_board_projection(
                &product.id,
                model,
                &mut boss_metrics::population_timing::PopulationTrace::disabled(),
                &crate::board_caches::BoardCaches::default(),
                now,
            )
            .unwrap();
        tree.tasks
            .iter()
            .chain(&tree.chores)
            .find(|card| card.id == item)
            .and_then(|card| card.auto_stop_notice.clone())
    };
    let raised = on_board(&db, 102, &mut model);
    assert_eq!(raised, stop_notice(&db, &item));
    assert!(raised.is_some(), "the incremental board carries the notice");
    check(&db, &product.id, &mut model, 103);

    acknowledge(&db, &item, raised.unwrap().audit_id);
    assert_eq!(on_board(&db, 104, &mut model), None);
    check(&db, &product.id, &mut model, 105);

    let replacement = ready_execution(&db, &item);
    raw_abandon(&db, &replacement.id);
    assert!(on_board(&db, 106, &mut model).is_some());
    check(&db, &product.id, &mut model, 107);

    let started = ready_execution(&db, &item);
    db.start_execution_run(&started.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert_eq!(on_board(&db, 108, &mut model), None);
    check(&db, &product.id, &mut model, 109);
}

#[test]
fn a_bypass_of_the_ledger_helpers_still_raises_the_notice() {
    let db = WorkDb::open(temp_db_path("auto-stop-unattributed")).unwrap();
    let product = create_test_product_named(&db, "Auto stop unattributed");
    let chore = create_test_chore_manual(&db, &product.id, "Raw");
    let execution = ready_execution(&db, &chore.id);
    raw_abandon(&db, &execution.id);
    let notice = stop_notice(&db, &chore.id).unwrap();
    assert_eq!(notice.reason_code, task_status_audit::UNATTRIBUTED_WRITE);
}

#[test]
fn a_pre_enrichment_autostart_off_derives_its_reason_from_basis() {
    let db = WorkDb::open(temp_db_path("auto-stop-legacy-autostart")).unwrap();
    let product = create_test_product_named(&db, "Auto stop legacy autostart");
    let bounced = create_test_chore_manual(&db, &product.id, "Bounced");
    let consumed = create_test_chore_manual(&db, &product.id, "Consumed");
    // First word is the writer; the rest is the socket caller, as on live rows.
    let bounce_basis = "cube_workspace_lease_failed pid=1";
    insert_pre_enrichment_autostart_off(&db, &bounced.id, bounce_basis);
    insert_pre_enrichment_autostart_off(&db, &consumed.id, task_status_audit::START_EXECUTION_RUN_BASIS);

    let bounced_row = db
        .list_task_field_audit(&bounced.id)
        .unwrap()
        .into_iter()
        .find(|row| row.property == "autostart" && row.old_value.as_deref() == Some("1"))
        .expect("the pre-enrichment 1→0 row is listed");
    assert!(bounced_row.reason_code.is_none());
    assert!(bounced_row.operation_id.is_none());
    assert_eq!(bounced_row.basis, bounce_basis);

    let notice = stop_notice(&db, &bounced.id).expect("a pre-enrichment bounce still raises");
    assert_eq!(notice.reason_code, "cube_workspace_lease_failed");
    assert!(notice.autostart_disabled && !notice.execution_abandoned);
    assert!(notice.audit_id > 0);

    assert_eq!(
        stop_notice(&db, &consumed.id),
        None,
        "a pre-enrichment run-start consumption is excluded by the derived code, with no later run"
    );
}
