//! `WorkDb::latest_local_execution_id_for_agent_id`: the durable slot-occupancy
//! query that gates `retire_pane`'s irreversible reap.

use super::*;

/// Start a run for a fresh chore under `product_id` on `agent_id` / `host_id`,
/// then pin its `work_runs.created_at` so ordering assertions do not depend on
/// clock resolution. Returns the execution id.
fn start_pinned_run_for_test(
    db: &WorkDb,
    product_id: &str,
    name: &str,
    agent_id: &str,
    host_id: &str,
    created_at: &str,
) -> String {
    let chore = create_test_chore_manual(db, product_id.to_owned(), name);
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    db.start_execution_run_on_host(&execution.id, agent_id, "mono", "lease-1", "ws-1", "/tmp/ws-1", host_id)
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET created_at = ?1 WHERE execution_id = ?2",
            rusqlite::params![created_at, &execution.id],
        )
        .unwrap();
    execution.id
}

#[test]
fn latest_local_execution_id_for_agent_id_ignores_remote_rows() {
    let db = WorkDb::open(temp_db_path("latest-local-exec-remote")).unwrap();
    let product = create_test_product_named(&db, "p");
    let local = start_pinned_run_for_test(&db, &product.id, "local", "worker-1", "local", "1000000000");
    // The remote row is NEWER, so only the host filter can exclude it.
    let _remote = start_pinned_run_for_test(&db, &product.id, "remote", "worker-1", "zakalwe", "1000000100");
    assert_eq!(
        db.latest_local_execution_id_for_agent_id("worker-1").unwrap(),
        Some(local),
    );
}

#[test]
fn latest_local_execution_id_for_agent_id_returns_the_newest_local_row() {
    let db = WorkDb::open(temp_db_path("latest-local-exec-newest")).unwrap();
    let product = create_test_product_named(&db, "p");
    let _older = start_pinned_run_for_test(&db, &product.id, "older", "worker-1", "local", "1000000000");
    let newer = start_pinned_run_for_test(&db, &product.id, "newer", "worker-1", "local", "1000000100");
    // A different slot's newer run must not leak in.
    let _other = start_pinned_run_for_test(&db, &product.id, "other", "worker-2", "local", "1000000200");
    assert_eq!(
        db.latest_local_execution_id_for_agent_id("worker-1").unwrap(),
        Some(newer),
    );
}

#[test]
fn latest_local_execution_id_for_agent_id_is_none_without_rows() {
    let db = WorkDb::open(temp_db_path("latest-local-exec-none")).unwrap();
    assert_eq!(db.latest_local_execution_id_for_agent_id("worker-9").unwrap(), None);
}
