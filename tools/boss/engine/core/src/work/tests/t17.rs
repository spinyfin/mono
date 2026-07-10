use super::*;

// Coverage for `WorkDb::set_dispatch_wait_reason` / `clear_dispatch_wait_reason`
// (the dispatch-wait surface added alongside `bossctl dispatch stats` — see
// `dispatch_reader::compute_wait_stats` for the read-side aggregation over
// the same `chain_serialized` / `pool_exhausted` reason vocabulary).

fn seed_ready_execution(db: &WorkDb, chore_id: &str) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(chore_id)
            .kind(ExecutionKind::TaskImplementation)
            .status(ExecutionStatus::Ready)
            .build(),
    )
    .unwrap()
}

#[test]
fn set_dispatch_wait_reason_stamps_reason_and_since() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = seed_ready_execution(&db, &chore_id);

    db.set_dispatch_wait_reason(&execution.id, "chain_serialized").unwrap();

    let reloaded = query_execution(&db.connect().unwrap(), &execution.id).unwrap().unwrap();
    assert_eq!(reloaded.dispatch_wait_reason.as_deref(), Some("chain_serialized"));
    assert!(reloaded.dispatch_wait_since.is_some());
}

#[test]
fn set_dispatch_wait_reason_preserves_since_when_reason_unchanged() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = seed_ready_execution(&db, &chore_id);

    db.set_dispatch_wait_reason(&execution.id, "pool_exhausted").unwrap();
    let first = query_execution(&db.connect().unwrap(), &execution.id)
        .unwrap()
        .unwrap()
        .dispatch_wait_since
        .unwrap();

    // Same reason on a later drain pass must not reset the start-of-wait
    // timestamp.
    db.set_dispatch_wait_reason(&execution.id, "pool_exhausted").unwrap();
    let second = query_execution(&db.connect().unwrap(), &execution.id)
        .unwrap()
        .unwrap()
        .dispatch_wait_since
        .unwrap();
    assert_eq!(first, second);
}

#[test]
fn set_dispatch_wait_reason_restamps_since_when_reason_changes() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = seed_ready_execution(&db, &chore_id);

    db.set_dispatch_wait_reason(&execution.id, "chain_serialized").unwrap();
    // Force the two stamps to land in different seconds so a changed
    // `since` is observable even on a fast test machine.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_since = '1' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();

    db.set_dispatch_wait_reason(&execution.id, "pool_exhausted").unwrap();
    let reloaded = query_execution(&db.connect().unwrap(), &execution.id).unwrap().unwrap();
    assert_eq!(reloaded.dispatch_wait_reason.as_deref(), Some("pool_exhausted"));
    assert_ne!(reloaded.dispatch_wait_since.as_deref(), Some("1"));
}

#[test]
fn clear_dispatch_wait_reason_nulls_both_columns() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = seed_ready_execution(&db, &chore_id);
    db.set_dispatch_wait_reason(&execution.id, "chain_serialized").unwrap();

    db.clear_dispatch_wait_reason(&execution.id).unwrap();

    let reloaded = query_execution(&db.connect().unwrap(), &execution.id).unwrap().unwrap();
    assert!(reloaded.dispatch_wait_reason.is_none());
    assert!(reloaded.dispatch_wait_since.is_none());
}
