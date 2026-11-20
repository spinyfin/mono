//! Persistence contracts for additive cancellation storage operations.
//!
//! These tests cover WorkDb methods that stamp `cancel_source` /
//! `cancel_actor` / `cancel_reason` in the same `UPDATE` that moves
//! execution status, and the post-commit dispatch-event flush. Engine
//! call sites keep writing unattributed rows until they migrate; there
//! is no schema trigger requiring provenance.

use super::*;
use boss_protocol::CancelSource;

fn stored_cancel_columns(db: &WorkDb, execution_id: &str) -> (Option<String>, Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT cancel_source, cancel_actor, cancel_reason FROM work_executions WHERE id = ?1",
            [execution_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
}

fn provenance_of(db: &WorkDb, execution_id: &str) -> boss_protocol::CancelProvenance {
    let (source, actor, reason) = stored_cancel_columns(db, execution_id);
    boss_protocol::CancelProvenance::new(
        source
            .expect("decision-terminal execution must store cancel_source")
            .parse::<CancelSource>()
            .expect("stored cancel_source must be a recognized source"),
        actor.expect("decision-terminal execution must store cancel_actor"),
        reason.expect("decision-terminal execution must store cancel_reason"),
    )
    .expect("stored cancel columns must form a provenance")
}

fn seed_ready_chore(db: &WorkDb, label: &str) -> (Task, WorkExecution) {
    let product = create_test_product(db);
    let chore = create_test_chore_manual(db, product.id.clone(), label);
    let execution = create_ready_chore_execution(db, chore.id.clone());
    (chore, execution)
}

#[test]
fn cancel_execution_with_provenance_records_the_supplied_columns() {
    let db = WorkDb::open(temp_db_path("cancel-prov-rpc")).unwrap();
    let (_, execution) = seed_ready_chore(&db, "rpc cancel");
    let provenance =
        boss_protocol::CancelProvenance::new(CancelSource::Bossctl, "operator", "wrong repo; stopping this run")
            .unwrap();

    let cancelled = db
        .cancel_execution_with_provenance(
            &execution.id,
            CancelExecutionOpts::default(),
            provenance.clone(),
            DecisionAuditContext::default(),
        )
        .unwrap();
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
    assert_eq!(provenance_of(&db, &execution.id), provenance);
}

#[test]
fn cancel_execution_without_provenance_leaves_columns_null() {
    let db = WorkDb::open(temp_db_path("cancel-prov-unattributed")).unwrap();
    let (_, execution) = seed_ready_chore(&db, "unattributed cancel");

    let cancelled = db.cancel_execution(&execution.id).unwrap();
    assert_eq!(cancelled.status, ExecutionStatus::Cancelled);
    assert_eq!(
        stored_cancel_columns(&db, &execution.id),
        (None, None, None),
        "existing callers must keep writing unattributed rows"
    );
}

#[test]
fn cancel_running_execution_with_provenance_preserves_supplied_columns() {
    let db = WorkDb::open(temp_db_path("cancel-prov-kanban")).unwrap();
    let (chore, execution) = seed_ready_chore(&db, "kanban cancel");
    db.start_execution_run(
        &execution.id,
        "worker-1",
        "mono",
        "lease-1",
        "mono-agent-001",
        "/tmp/mono-agent-001",
    )
    .unwrap();
    let provenance = boss_protocol::CancelProvenance::engine(
        CancelSource::WorkItemMovedToBacklog,
        "session abc",
        format!(
            "work item {} moved from active back to todo (kanban drag to Backlog)",
            chore.id
        ),
    );

    assert!(
        db.cancel_running_execution_with_provenance(&execution.id, provenance.clone())
            .unwrap()
    );
    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Cancelled
    );
    assert_eq!(provenance_of(&db, &execution.id), provenance);
}

#[tokio::test]
async fn mark_execution_redundant_with_provenance_records_columns_and_dispatch_event() {
    let sink = std::sync::Arc::new(boss_dispatch_events::RecordingDispatchEventSink::new());
    let db = WorkDb::open(temp_db_path("cancel-prov-redundant"))
        .unwrap()
        .with_dispatch_events(sink.clone());
    let (_, execution) = seed_ready_chore(&db, "redundant");
    let provenance = boss_protocol::CancelProvenance::engine(
        CancelSource::DoubleSpawnGuard,
        "execution_coordinator",
        format!(
            "redundant duplicate: live execution other already holds work item {}",
            execution.work_item_id
        ),
    );

    db.mark_execution_redundant_with_provenance(&execution.id, provenance.clone())
        .unwrap();
    let after = db.get_execution(&execution.id).unwrap();
    assert_eq!(after.status, ExecutionStatus::Abandoned);
    assert_eq!(provenance_of(&db, &execution.id), provenance);
    let events = sink.events().await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.execution_id, execution.id);
    assert_eq!(event.details["work_item_id"], execution.work_item_id);
    assert_eq!(event.details["to_status"], "abandoned");
    assert_eq!(event.details["source"], provenance.source.as_str());
    assert_eq!(event.details["actor"], provenance.actor);
    assert_eq!(event.details["reason"], provenance.reason);
}

#[tokio::test]
async fn cancel_execution_with_provenance_reaches_the_sink_only_after_commit() {
    let sink = std::sync::Arc::new(boss_dispatch_events::RecordingDispatchEventSink::new());
    let db = WorkDb::open(temp_db_path("cancel-prov-post-commit"))
        .unwrap()
        .with_dispatch_events(sink.clone());
    let (_, execution) = seed_ready_chore(&db, "post-commit");
    let provenance = boss_protocol::CancelProvenance::new(CancelSource::Bossctl, "operator", "stop this run").unwrap();

    assert!(
        sink.events().await.is_empty(),
        "no provenance must be recorded before the cancel write"
    );
    db.cancel_execution_with_provenance(
        &execution.id,
        CancelExecutionOpts::default(),
        provenance.clone(),
        DecisionAuditContext::default(),
    )
    .unwrap();
    let events = sink.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].execution_id, execution.id);
    assert_eq!(events[0].details["source"], provenance.source.as_str());
    assert_eq!(events[0].details["actor"], provenance.actor);
    assert_eq!(events[0].details["reason"], provenance.reason);
    assert_eq!(events[0].details["to_status"], "cancelled");
}

#[test]
fn decision_terminal_update_without_provenance_still_succeeds() {
    let db = WorkDb::open(temp_db_path("cancel-prov-no-trigger")).unwrap();
    let (_, execution) = seed_ready_chore(&db, "no trigger");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'cancelled' WHERE id = ?1",
            [&execution.id],
        )
        .expect("unattributed decision-terminal writes must remain valid until callers migrate");
    }

    assert_eq!(
        db.get_execution(&execution.id).unwrap().status,
        ExecutionStatus::Cancelled
    );
    assert_eq!(stored_cancel_columns(&db, &execution.id), (None, None, None));
}
