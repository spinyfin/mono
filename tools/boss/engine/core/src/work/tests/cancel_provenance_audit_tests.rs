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

/// The cancel is also published (after commit) for the dispatch timeline, so
/// `bossctl dispatch diagnose` does not stop short of a cancelled row. The
/// timeline subscriber turns exactly this event into the dispatch event.
async fn next_cancel_event(sub: &mut boss_event_bus::Subscription) -> boss_event_bus::Event {
    tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
        .await
        .expect("a cancel event is published after commit")
        .expect("bus open")
}

#[tokio::test]
async fn a_dependency_block_cancel_reaches_the_timeline_publisher() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    db.start_execution_run(&execution.id, "worker-1", "cube-repo", "lease", "ws", "/tmp/ws")
        .unwrap();
    let prerequisite = create_test_chore_manual(&db, product_id, "Prerequisite");
    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionCancelled,
    ));

    db.add_dependency_with_worker_reconcile(AddDependencyInput {
        dependent: chore_id,
        prerequisite: prerequisite.id,
        relation: None,
    })
    .unwrap();

    assert_cancel_diagnose(&db, &execution.id).await;
    let event = next_cancel_event(&mut sub).await;
    let timeline = crate::cancel_timeline::timeline_event(&event).expect("no caller records this cancel");
    assert_eq!(timeline.execution_id, execution.id);
    assert_eq!(timeline.details["actor"], "engine");
    assert_eq!(timeline.details["reason"], "dependent transitioned to blocked");
    assert_eq!(timeline.details["prior_status"], "running");
}

#[tokio::test]
async fn a_remediation_tombstone_cancel_reaches_the_timeline_publisher() {
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id.clone());
    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionCancelled,
    ));

    db.test_tombstone_orphaned_remediation(&chore_id).unwrap();

    assert_cancel_diagnose(&db, &execution.id).await;
    let event = next_cancel_event(&mut sub).await;
    let timeline = crate::cancel_timeline::timeline_event(&event).expect("no caller records this cancel");
    assert_eq!(timeline.execution_id, execution.id);
    assert_eq!(timeline.details["reason"], "orphaned review remediation tombstoned");
}

/// A caller that records the timeline event itself (the RPC handler, the
/// coordinator) is not recorded a second time by the subscriber, and its
/// `queued_only` / `peer_pid` reach the audit record.
#[test]
fn a_caller_recorded_cancel_keeps_queued_only_and_peer_pid_in_the_audit_record() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    let (db, _product_id, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id);
    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionCancelled,
    ));

    db.cancel_execution_with(
        &execution.id,
        CancelExecutionOpts {
            queued_only: true,
            peer_pid: Some(4242),
            caller_records_timeline: true,
            ..CancelExecutionOpts::default()
        },
    )
    .unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let event = runtime.block_on(next_cancel_event(&mut sub));
    assert!(crate::cancel_timeline::timeline_event(&event).is_none());
    let records = audit_records_for(&execution.id);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0]["queued_only"], true);
    assert_eq!(records[0]["peer_pid"], 4242);
}

/// The recovery re-arm retires shadowed executions with a stated reason on
/// the row and an audit record, written once the migration commits.
#[tokio::test]
async fn recovery_rearm_cancels_record_a_reason_on_the_execution() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("work.db");
    let (id, pending_id) = {
        let db = WorkDb::open(path.clone()).unwrap();
        let product = create_test_product(&db);
        let stale = format!(
            "{} boss-base/exec_failed",
            boss_engine_recovery::execution_bookmark::NOT_DESCENDED_MESSAGE
        );
        let chore = create_test_chore_manual(&db, product.id.clone(), "wedged");
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'execution_recovery_failed', blocked_detail = ?2 WHERE id = ?1",
            params![chore.id, stale],
        )
        .unwrap();
        drop(conn);
        let pending = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(chore.id.clone())
                    .kind(ExecutionKind::ChoreImplementation)
                    .status(ExecutionStatus::WaitingDependency)
                    .build(),
            )
            .unwrap();
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore.id.clone())
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Failed)
                .build(),
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute_batch(
                "DROP INDEX work_executions_in_flight_idx; ALTER TABLE ci_remediations DROP COLUMN requeue_enqueued_at;
                 ALTER TABLE ci_remediations DROP COLUMN queue_base_sha;
                 ALTER TABLE execution_bookmarks DROP COLUMN base_commit;
                 ALTER TABLE execution_bookmarks DROP COLUMN reconciliation_note;
                 ALTER TABLE execution_bookmarks DROP COLUMN retired_at;
                 ALTER TABLE execution_bookmarks DROP COLUMN retired_reason;
                 ALTER TABLE tasks DROP COLUMN pr_updated_at;
                 UPDATE metadata SET value = '38' WHERE key = 'schema_version';",
            )
            .unwrap();
        (chore.id, pending.id)
    };
    let db = {
        let _audit = crate::audit::lock_audit_globals_for_tests();
        let db = WorkDb::open(path).unwrap();
        let conn = db.connect().unwrap();
        let row = query_execution(&conn, &pending_id).unwrap().unwrap();
        assert_eq!(row.status, ExecutionStatus::Cancelled);
        assert!(
            row.last_error
                .as_deref()
                .is_some_and(|text| text.contains("recovery re-arm")),
            "the retired row says why: {:?} ({id})",
            row.last_error
        );
        drop(conn);
        let records = audit_records_for(&pending_id);
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0]["actor"], "engine");
        assert_eq!(records[0]["from_status"], "waiting_dependency");
        db
    };
    assert_cancel_diagnose(&db, &pending_id).await;
}

async fn assert_cancel_diagnose(db: &WorkDb, execution_id: &str) {
    let dir = tempfile::tempdir().unwrap();
    let sink = crate::dispatch_events::JsonlFileSink::new(dir.path());
    crate::cancel_timeline::drain(db, &sink).await.unwrap();
    let events = crate::dispatch_reader::read_execution(dir.path(), execution_id)
        .unwrap()
        .events;
    assert_eq!(events.len(), 1, "committed cancel is on the diagnostic read surface");
    assert_eq!(events[0].stage, "execution_cancelled");
    assert_eq!(events[0].details["actor"], "engine");
    assert!(!events[0].details["reason"].as_str().unwrap().is_empty());
}

#[test]
fn rolled_back_cancel_leaves_no_audit_or_durable_event() {
    let _audit = crate::audit::lock_audit_globals_for_tests();
    let (db, _, chore_id) = setup_product_and_chore();
    let execution = create_ready_chore_execution(&db, chore_id);
    {
        let mut conn = db.connect().unwrap();
        let tx = conn.transaction().unwrap();
        let mut pending = PendingEvents::new();
        crate::work::executions_runs::cancel_execution_in_tx(
            &tx,
            &mut pending,
            &execution.id,
            CancelExecutionOpts::default(),
        )
        .unwrap();
        pending.persist_cancels(&tx).unwrap();
        // Dropping the transaction must discard both state and provenance.
    }
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);
    assert!(db.pending_cancel_events().unwrap().is_empty());
    assert!(audit_records_for(&execution.id).is_empty());
}
