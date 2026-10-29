use super::*;
use crate::work::task_status_audit;

fn question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Change the timeout?".into(),
        explanation: "The test needs a longer timeout.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    }
}

#[test]
fn ledger_is_atomic_for_bulk_updates_and_failed_statements() {
    let db = WorkDb::open(temp_db_path("audit-atomic")).unwrap();
    let product = create_test_product_named(&db, "Atomic");
    let a = create_test_chore_manual(&db, &product.id, "First");
    let b = create_test_chore_manual(&db, &product.id, "Second");
    let mut conn = db.connect().unwrap();
    let count = |c: &Connection| {
        c.query_row("SELECT COUNT(*) FROM task_status_audit", [], |r| r.get::<_, i64>(0))
            .unwrap()
    };
    let before = count(&conn);
    {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        task_status_audit::execute(
            &tx,
            "bulk_test",
            "human",
            "UPDATE tasks SET autostart = 1 WHERE id IN (?1, ?2)",
            params![a.id, b.id],
        )
        .unwrap();
        assert_eq!(count(&tx), before + 2);
        tx.rollback().unwrap();
    }
    assert_eq!(count(&conn), before);
    assert!(
        task_status_audit::execute(
            &conn,
            "bad_test",
            "engine",
            "UPDATE tasks SET status = 'invalid' WHERE id = ?1",
            [&a.id]
        )
        .is_err()
    );
    assert_eq!(count(&conn), before);
    let context: i64 = conn
        .query_row("SELECT COUNT(*) FROM task_status_audit_context", [], |r| r.get(0))
        .unwrap();
    assert_eq!(context, 0);
    // FAIL retains changes made before the trigger error unless the helper
    // rolls back its savepoint (SQLite's ordinary ABORT already undoes them).
    conn.execute_batch(
        "CREATE TRIGGER fail_after_write AFTER UPDATE OF autostart ON tasks
        BEGIN SELECT RAISE(FAIL, 'failed after row update'); END;",
    )
    .unwrap();
    assert!(
        task_status_audit::execute(
            &conn,
            "partial_failure",
            "engine",
            "UPDATE tasks SET autostart = 1 WHERE id = ?1",
            [&a.id],
        )
        .is_err()
    );
    assert_eq!(count(&conn), before);
    let autostart: bool = conn
        .query_row("SELECT autostart FROM tasks WHERE id = ?1", [&a.id], |r| r.get(0))
        .unwrap();
    assert!(!autostart);
    conn.execute_batch("DROP TRIGGER fail_after_write").unwrap();
    assert_eq!(
        task_status_audit::execute(
            &conn,
            "bulk_test",
            "human",
            "UPDATE tasks SET autostart = 1 WHERE id IN (?1, ?2)",
            params![a.id, b.id],
        )
        .unwrap(),
        2
    );
    let recorded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE property = 'autostart'
         AND old_value = '0' AND new_value = '1' AND actor = 'human' AND basis = 'bulk_test'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 2);
    assert_eq!(count(&conn), before + 2);
}

#[test]
fn raw_sql_writes_are_recorded_and_unchanged_values_do_not_reset_age() {
    let db = WorkDb::open(temp_db_path("audit-raw")).unwrap();
    let product = create_test_product_named(&db, "Raw SQL");
    let task = create_test_chore_manual(&db, &product.id, "Raw write");
    let conn = db.connect().unwrap();
    let initial: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND old_value IS NULL
         AND basis = 'insert_chore_with_project_in_tx_as_actor' AND actor = 'engine' AND age_is_lower_bound = 0",
            params![task.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        initial, 4,
        "insert records each tracked property, including NULL reasons"
    );
    conn.execute(
        "UPDATE tasks SET status = 'active', last_status_actor = 'engine' WHERE id = ?1",
        [&task.id],
    )
    .unwrap();
    let (old, new, actor, basis, at): (String, String, String, String, String) = conn
        .query_row(
            "SELECT old_value, new_value, actor, basis, changed_at FROM task_status_audit
         WHERE work_item_id = ?1 AND property = 'status' ORDER BY id DESC LIMIT 1",
            [&task.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!((old.as_str(), new.as_str()), ("todo", "active"));
    assert_eq!((actor.as_str(), basis.as_str()), ("engine", "unattributed_write"));
    assert!(at.parse::<i64>().unwrap() > 0);
    conn.execute(
        "UPDATE tasks SET status = status, blocked_reason = blocked_reason,
         dispatch_failed_reason = dispatch_failed_reason, autostart = autostart, description = 'edited'
         WHERE id = ?1",
        [&task.id],
    )
    .unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1",
            [&task.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 5);
}

#[test]
fn repeated_failure_and_clear_then_fail_do_not_reuse_old_provenance() {
    let db = WorkDb::open(temp_db_path("audit-repeated")).unwrap();
    let product = create_test_product_named(&db, "Repeated failures");
    let task = create_test_chore_manual(&db, &product.id, "Retry");
    let conn = db.connect().unwrap();
    for (basis, reason) in [
        ("declared_blocked", Some("worker_failed")),
        ("nudge_breaker", Some("worker_failed")),
        ("explicit_start_failure_clear", None),
        ("foreign_pr_binding", Some("worker_failed")),
    ] {
        task_status_audit::execute(
            &conn,
            basis,
            "engine",
            "UPDATE tasks SET blocked_reason = ?2 WHERE id = ?1",
            params![task.id, reason],
        )
        .unwrap();
    }
    task_status_audit::execute(
        &conn,
        "unrelated",
        "human",
        "UPDATE tasks SET autostart = 1 WHERE id = ?1",
        [&task.id],
    )
    .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT basis FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason' ORDER BY id",
        )
        .unwrap();
    let bases = stmt
        .query_map([&task.id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        bases,
        [
            "insert_chore_with_project_in_tx_as_actor",
            "declared_blocked",
            "nudge_breaker",
            "explicit_start_failure_clear",
            "foreign_pr_binding"
        ]
    );
    for at in ["100", "200", "200"] {
        conn.execute(
            "UPDATE tasks SET dispatch_failed_reason = 'lease_failed', dispatch_failed_at = ?2 WHERE id = ?1",
            params![task.id, at],
        )
        .unwrap();
    }
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'dispatch_failed_reason'",
            [&task.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 3, "insert plus two distinct failure observations");
}

#[test]
fn legacy_backfill_recovers_current_evidence_and_keeps_unknown_failures_visible() {
    for producer in [
        "declared_blocked",
        "nudge_breaker",
        "foreign_pr_binding",
        "delivered_without_pr",
        "operator_no",
        "unknown_legacy",
    ] {
        let db = WorkDb::open(temp_db_path(producer)).unwrap();
        let (_, id, execution) = make_waiting_human_chore(&db, producer);
        if producer == "operator_no" {
            let q = question();
            crate::work::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
            db.record_worker_awaiting_operator_answer(&execution, "needs decision", &q)
                .unwrap();
            // Historical shape: a declined answer used to park as worker_failed.
            {
                let conn = db.connect().unwrap();
                conn.execute(
                    "UPDATE operator_questions SET status = 'answered', answered_by = 'human',
                 answer_json = '{\"kind\":\"yes_no\",\"value\":false}', answered_at = '200'
                 WHERE execution_id = ?1",
                    [&execution],
                )
                .unwrap();
                conn.execute(
                    "UPDATE tasks SET blocked_reason = 'worker_failed', blocked_detail = 'Operator declined'
                 WHERE id = ?1",
                    [&id],
                )
                .unwrap();
                conn.execute(
                    "UPDATE work_executions SET status = 'failed', finished_at = '200' WHERE id = ?1",
                    [&execution],
                )
                .unwrap();
            }
        } else {
            db.record_worker_failure(&execution, "Operator declined (untrusted prose)")
                .unwrap();
        }
        match producer {
            "declared_blocked" | "delivered_without_pr" => {
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE work_executions SET run_done_outcome = ?2 WHERE id = ?1",
                        params![
                            execution,
                            if producer == "declared_blocked" {
                                "blocked"
                            } else {
                                "delivered"
                            }
                        ],
                    )
                    .unwrap();
            }
            "nudge_breaker" | "foreign_pr_binding" => {
                db.create_attention_item(
                    CreateAttentionItemInput::builder()
                        .execution_id(&execution)
                        .kind(if producer == "nudge_breaker" {
                            "nudge_breaker_tripped"
                        } else {
                            "foreign_pr_binding_refused"
                        })
                        .title("failure")
                        .body_markdown("evidence")
                        .build(),
                )
                .unwrap();
            }
            _ => {}
        }
        let conn = db.connect().unwrap();
        conn.execute("DELETE FROM task_status_audit WHERE work_item_id = ?1", [&id])
            .unwrap();
        crate::work::task_status_backfill::backfill(&conn).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_status_audit", [], |r| r.get(0))
            .unwrap();
        crate::work::task_status_backfill::backfill(&conn).unwrap();
        assert_eq!(
            before,
            conn.query_row("SELECT COUNT(*) FROM task_status_audit", [], |r| r.get::<_, i64>(0))
                .unwrap()
        );
        let (basis, lower_bound): (String, bool) = conn.query_row(
            "SELECT basis, age_is_lower_bound FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'",
            [&id], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(basis, producer);
        assert_eq!(lower_bound, producer == "unknown_legacy");
    }
}

#[test]
fn legacy_backfill_does_not_reuse_an_older_execution_refusal() {
    let db = WorkDb::open(temp_db_path("audit-old-refusal")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Old refusal");
    let q = question();
    crate::work::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "decision", &q)
        .unwrap();
    // Historical shape: a declined answer used to park as worker_failed.
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE operator_questions SET status = 'answered', answered_by = 'human',
         answer_json = '{\"kind\":\"yes_no\",\"value\":false}', answered_at = '200'
         WHERE execution_id = ?1",
            [&execution],
        )
        .unwrap();
        conn.execute(
            "UPDATE tasks SET blocked_reason = 'worker_failed', blocked_detail = 'Operator declined'
         WHERE id = ?1",
            [&id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'failed', finished_at = '200' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    }
    let later = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();
    db.record_worker_failure(&later.id, "unknown historical cause").unwrap();
    let conn = db.connect().unwrap();
    conn.execute("DELETE FROM task_status_audit WHERE work_item_id = ?1", [&id])
        .unwrap();
    crate::work::task_status_backfill::backfill(&conn).unwrap();
    let (basis, lower_bound): (String, bool) = conn.query_row(
        "SELECT basis, age_is_lower_bound FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'",
        [&id], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!(basis, "unknown_legacy");
    assert!(lower_bound);
}

#[test]
fn upgrading_existing_tasks_backfills_once_and_preserves_known_dispatch_age() {
    for version in [Some(58), Some(59), Some(60), None] {
        let (_dir, path) = disk_db_path("audit-upgrade");
        let db = WorkDb::open(path.clone()).unwrap();
        let product = create_test_product_named(&db, "Upgrade");
        let task = create_test_chore_manual(&db, &product.id, "Existing task");
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET dispatch_failed_reason = 'deliberate_park', dispatch_failed_at = '12345' WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
        for property in ["status", "blocked_reason", "dispatch_failed_reason", "autostart"] {
            conn.execute_batch(&format!(
                "DROP TRIGGER task_status_audit_insert_{property}; DROP TRIGGER task_status_audit_update_{property};"
            ))
            .unwrap();
        }
        conn.execute_batch("DROP TRIGGER execution_waiting_human_insert; DROP TRIGGER execution_waiting_human_update; DELETE FROM schema_migrations WHERE id = '20261029_1900_execution_waiting_human_history';").unwrap();
        conn.execute_batch("DROP TABLE task_status_audit; DROP TABLE task_status_audit_context;")
            .unwrap();
        if let Some(version) = version {
            if version < 60 {
                conn.execute_batch(
                    "ALTER TABLE execution_bookmarks DROP COLUMN prune_checked_at;
                     ALTER TABLE execution_bookmarks DROP COLUMN prune_outcome;",
                )
                .unwrap();
            }
            conn.execute_batch("DROP TABLE schema_migrations").unwrap();
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)",
                [version.to_string()],
            )
            .unwrap();
        } else {
            conn.execute(
                "DELETE FROM schema_migrations WHERE id = ?1",
                [task_status_audit::MIGRATION_ID],
            )
            .unwrap();
        }
        drop(conn);
        drop(db);
        for _ in 0..2 {
            let reopened = WorkDb::open(path.clone()).unwrap();
            let conn = reopened.connect().unwrap();
            schema_init::assert_all_migrations_applied(&conn);
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1",
                    [&task.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 4);
            let known: i64 = conn.query_row(
                "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'dispatch_failed_reason'
                 AND new_value = 'deliberate_park' AND basis = 'deliberate_park' AND changed_at = '12345' AND age_is_lower_bound = 0",
                [&task.id], |r| r.get(0),
            ).unwrap();
            assert_eq!(known, 1);
            let unknown: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND basis = 'unknown_legacy'
                 AND age_is_lower_bound = 1 AND old_value IS NULL",
                    [&task.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(unknown, 3);
        }
    }
}
