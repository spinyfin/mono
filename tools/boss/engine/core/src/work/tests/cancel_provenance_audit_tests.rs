//! Every path that terminalizes an execution as `cancelled` leaves a durable
//! `execution_cancelled` record in `engine-audit.log` naming the actor and
//! reason, not just the app-level cancel handlers.

use super::*;

fn audit_records_for(execution_id: &str) -> Vec<serde_json::Value> {
    let path = crate::audit::default_audit_log_path().expect("a test process resolves its isolated audit path");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|record| record["event"] == "execution_cancelled" && record["execution_id"] == execution_id)
        .collect()
}

#[test]
fn a_dependency_block_records_the_engine_cancel() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
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

    let records = audit_records_for(&execution.id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["actor"], "engine");
    assert_eq!(records[0]["reason"], "dependent transitioned to blocked");
    assert_eq!(records[0]["from_status"], "running");
}

#[test]
fn a_remediation_tombstone_records_the_engine_cancel() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());

    db.test_tombstone_orphaned_remediation(&chore_id).unwrap();

    let records = audit_records_for(&execution.id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["actor"], "engine");
    assert_eq!(records[0]["reason"], "orphaned review remediation tombstoned");
    assert_eq!(records[0]["from_status"], "ready");
}

#[test]
fn a_work_db_cancel_records_the_named_actor_and_reason() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id);

    db.cancel_execution_with(
        &execution.id,
        CancelExecutionOpts {
            reason: Some("coordinator cancelled it".to_owned()),
            actor: StatusActor::Boss,
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();

    let records = audit_records_for(&execution.id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["actor"], "boss");
    assert_eq!(records[0]["reason"], "coordinator cancelled it");
    assert_eq!(records[0]["work_item_moved"], true);
}
