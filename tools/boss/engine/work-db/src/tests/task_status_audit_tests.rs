use super::*;
use crate::task_status_audit;

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
        initial, 5,
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
    assert_eq!(count, 6);
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
            crate::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
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
        crate::task_status_backfill::backfill(&conn).unwrap();
        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM task_status_audit", [], |r| r.get(0))
            .unwrap();
        crate::task_status_backfill::backfill(&conn).unwrap();
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
    crate::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
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
    crate::task_status_backfill::backfill(&conn).unwrap();
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
        for property in [
            "status",
            "blocked_reason",
            "dispatch_failed_reason",
            "autostart",
            "deferred",
        ] {
            conn.execute_batch(&format!(
                "DROP TRIGGER task_status_audit_insert_{property}; DROP TRIGGER task_status_audit_update_{property};"
            ))
            .unwrap();
        }
        crate::progression_candidates::drop_views(&conn).unwrap();
        conn.execute_batch(
            "DROP TRIGGER task_status_audit_abandon_execution;
             DROP TABLE task_status_audit; DROP TABLE task_status_audit_context;
             DROP TABLE task_status_audit_operation;",
        )
        .unwrap();
        if let Some(version) = version {
            conn.execute_batch(
                "DROP TABLE dispatch_start_failures;
                 ALTER TABLE work_executions DROP COLUMN dispatch_not_before_source;",
            )
            .unwrap();
            if version < 60 {
                // Schema 60's columns were dropped by a later migration, so an
                // upgraded database no longer has them to remove.
                for column in ["prune_checked_at", "prune_outcome"] {
                    if crate::schema_init::table_has_column(&conn, "execution_bookmarks", column).unwrap() {
                        conn.execute_batch(&format!("ALTER TABLE execution_bookmarks DROP COLUMN {column}"))
                            .unwrap();
                    }
                }
            }
            conn.execute_batch("DROP TABLE schema_migrations").unwrap();
            conn.execute(
                "INSERT INTO metadata (key, value) VALUES ('schema_version', ?1)",
                [version.to_string()],
            )
            .unwrap();
        } else {
            conn.execute(
                "DELETE FROM schema_migrations WHERE id IN (?1, ?2, ?3)",
                [
                    task_status_audit::MIGRATION_ID,
                    task_status_audit::DEFERRED_MIGRATION_ID,
                    task_status_audit::EXECUTION_ABANDON_MIGRATION_ID,
                ],
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
            assert_eq!(count, 5, "one baseline row per tracked property");
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
            assert_eq!(unknown, 4);
        }
    }
}

// ── `deferred` / `autostart` history ───────────────────────────────────────
//
// Every writer of the two flags must leave a ledger row naming the actor and
// the code path. These tests drive one writer from each category through its
// public entry point and read the result back the way `boss task show` does.

/// `(old, new, actor, basis)` for each recorded change to `property`, oldest first.
fn flag_history(db: &WorkDb, id: &str, property: &str) -> Vec<(Option<String>, Option<String>, String, String)> {
    db.list_task_field_audit(id)
        .unwrap()
        .into_iter()
        .filter(|row| row.property == property)
        .map(|row| (row.old_value, row.new_value, row.actor, row.basis))
        .collect()
}

fn history_row(
    old: Option<&str>,
    new: &str,
    actor: &str,
    basis: &str,
) -> (Option<String>, Option<String>, String, String) {
    (
        old.map(str::to_owned),
        Some(new.to_owned()),
        actor.to_owned(),
        basis.to_owned(),
    )
}

#[test]
fn creation_paths_record_both_flags_with_the_creating_actor_and_path() {
    let db = WorkDb::open(temp_db_path("flag-audit-create")).unwrap();
    let product = create_test_product_named(&db, "Flag create");
    let chore = db
        .create_chore_as_actor(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name("Deferred chore")
                .autostart(false)
                .deferred(true)
                .build(),
            "boss",
        )
        .unwrap();
    let basis = "insert_chore_with_project_in_tx_as_actor";
    assert_eq!(
        flag_history(&db, &chore.id, "deferred"),
        [history_row(None, "1", "boss", basis)]
    );
    assert_eq!(
        flag_history(&db, &chore.id, "autostart"),
        [history_row(None, "0", "boss", basis)]
    );

    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Flag project")
                .goal("Hold a task")
                .build(),
        )
        .unwrap();
    let task = db
        .create_task_as_actor(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Deferred task")
                .deferred(true)
                .build(),
            "human",
        )
        .unwrap();
    let basis = "insert_task_in_tx_as_actor";
    assert_eq!(
        flag_history(&db, &task.id, "deferred"),
        [history_row(None, "1", "human", basis)]
    );
    assert_eq!(
        flag_history(&db, &task.id, "autostart"),
        [history_row(None, "1", "human", basis)]
    );
}

#[test]
fn patch_writes_record_the_callers_actor_and_named_or_default_basis() {
    let db = WorkDb::open(temp_db_path("flag-audit-patch")).unwrap();
    let product = create_test_product_named(&db, "Flag patch");
    let chore = create_test_chore_manual(&db, &product.id, "Patched");
    let patch = |deferred: Option<bool>, autostart: Option<bool>| WorkItemPatch {
        deferred,
        autostart,
        ..Default::default()
    };

    // A socket request names the handler and the calling process.
    let socket_basis = "rpc:update_work_item peer=socket_client pid=4242 session=s1 request=r1";
    db.update_work_item_with_basis(&chore.id, patch(Some(true), Some(true)), "human", socket_basis)
        .unwrap();
    // A caller that does not name itself gets the default path name.
    db.update_work_item_as_actor(&chore.id, patch(Some(false), None), "boss")
        .unwrap();
    db.update_work_item(&chore.id, patch(Some(true), Some(false))).unwrap();
    // Re-asserting the current value is not a change and records nothing.
    db.update_work_item(&chore.id, patch(Some(true), Some(false))).unwrap();

    assert_eq!(
        flag_history(&db, &chore.id, "deferred"),
        [
            history_row(None, "0", "engine", "insert_chore_with_project_in_tx_as_actor"),
            history_row(Some("0"), "1", "human", socket_basis),
            history_row(Some("1"), "0", "boss", "update_task"),
            history_row(Some("0"), "1", "human", "update_task"),
        ]
    );
    assert_eq!(
        flag_history(&db, &chore.id, "autostart"),
        [
            history_row(None, "0", "engine", "insert_chore_with_project_in_tx_as_actor"),
            history_row(Some("0"), "1", "human", socket_basis),
            history_row(Some("1"), "0", "human", "update_task"),
        ]
    );
}

#[test]
fn explicit_start_records_the_deferred_clear() {
    let db = WorkDb::open(temp_db_path("flag-audit-start")).unwrap();
    let product = create_test_product_named(&db, "Flag start");
    let defer = |db: &WorkDb, id: &str| {
        db.update_work_item(
            id,
            WorkItemPatch {
                deferred: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
    };

    let automatic = create_test_chore(&db, &product.id, "Automatic start");
    defer(&db, &automatic.id);
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(automatic.id.clone())
            .build(),
    )
    .unwrap();
    assert_eq!(
        flag_history(&db, &automatic.id, "deferred").last(),
        Some(&history_row(Some("1"), "0", "engine", "explicit_start_clears_deferred"))
    );

    let human = create_test_chore(&db, &product.id, "Human start");
    defer(&db, &human.id);
    let provenance = "rpc:request_execution peer=socket_client pid=4242 session=s1 request=r1";
    db.request_execution(
        RequestExecutionInput::builder()
            .work_item_id(human.id.clone())
            .audit_actor("human")
            .audit_basis(provenance)
            .build(),
    )
    .unwrap();
    assert_eq!(
        flag_history(&db, &human.id, "deferred").last(),
        Some(&history_row(
            Some("1"),
            "0",
            "human",
            &format!("explicit_start_clears_deferred {provenance}")
        ))
    );
}

#[test]
fn reconcile_active_dispatch_names_itself_when_clearing_deferred() {
    let db = WorkDb::open(temp_db_path("flag-audit-reconcile-deferred")).unwrap();
    let product = create_test_product_named(&db, "Flag reconcile deferred");
    let chore = create_test_chore(&db, &product.id, "Active and deferred");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            deferred: Some(true),
            ..Default::default()
        },
    )
    .unwrap();

    let redispatched = db.reconcile_active_dispatch(|_| true).unwrap();
    assert_eq!(redispatched, vec![chore.id.clone()]);
    assert_eq!(
        flag_history(&db, &chore.id, "deferred").last(),
        Some(&history_row(
            Some("1"),
            "0",
            "engine",
            "explicit_start_clears_deferred reconcile_active_dispatch"
        ))
    );
}

#[test]
fn engine_dispatch_paths_record_their_autostart_changes() {
    let db = WorkDb::open(temp_db_path("flag-audit-engine")).unwrap();
    let product = create_test_product_named(&db, "Flag engine");

    // A dispatch failure parks the item, then the recovery sweep re-arms it.
    let bounced = create_test_chore(&db, &product.id, "Bounced");
    assert!(
        db.bounce_dispatch_failed_to_backlog(&bounced.id, "lease_failed", "no workspace")
            .unwrap()
    );
    db.reenable_and_request_execution_with_live_check(
        &bounced.id,
        RequestExecutionInput::builder()
            .work_item_id(bounced.id.clone())
            .build(),
        |_| true,
    )
    .unwrap();
    assert_eq!(
        flag_history(&db, &bounced.id, "autostart")[1..],
        [
            history_row(Some("1"), "0", "engine", "lease_failed"),
            history_row(
                Some("0"),
                "1",
                "engine",
                "reenable_autostart_after_dispatch_failure_in_tx"
            ),
        ]
    );

    // Starting a run consumes the one-shot autostart.
    let started = create_test_chore(&db, &product.id, "Started");
    let execution = create_ready_chore_execution(&db, started.id.clone());
    db.start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    assert_eq!(
        flag_history(&db, &started.id, "autostart").last(),
        Some(&history_row(
            Some("1"),
            "0",
            "engine",
            "start_execution_run_on_host_with_tmux_hosting"
        ))
    );

    // Progression parking an item behind a question clears it under its own actor.
    let asked = create_test_chore(&db, &product.id, "Asked");
    {
        let conn = db.connect().unwrap();
        crate::progression_done::park_awaiting_answer(&conn, &asked.id, "Which way?", "progression_question").unwrap();
    }
    assert_eq!(
        flag_history(&db, &asked.id, "autostart").last(),
        Some(&history_row(Some("1"), "0", "progression", "progression_question"))
    );
}

#[test]
fn a_write_that_bypasses_the_helper_is_still_recorded_as_unattributed() {
    let db = WorkDb::open(temp_db_path("flag-audit-raw")).unwrap();
    let product = create_test_product_named(&db, "Flag raw");
    let chore = create_test_chore_manual(&db, &product.id, "Raw");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deferred = 1, autostart = 1 WHERE id = ?1",
            [&chore.id],
        )
        .unwrap();
    assert_eq!(
        flag_history(&db, &chore.id, "deferred").last(),
        Some(&history_row(Some("0"), "1", "human", "unattributed_write"))
    );
    assert_eq!(
        flag_history(&db, &chore.id, "autostart").last(),
        Some(&history_row(Some("0"), "1", "human", "unattributed_write"))
    );
}

#[test]
fn field_audit_listing_reports_flags_and_correlated_status_oldest_first() {
    let db = WorkDb::open(temp_db_path("flag-audit-list")).unwrap();
    let product = create_test_product_named(&db, "Flag list");
    let chore = create_test_chore_manual(&db, &product.id, "Listed");
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            status: Some("active".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let rows = db.list_task_field_audit(&chore.id).unwrap();
    // The status move rode in the same operation as the deferral, so it is
    // reported with it; the item's earlier, unrelated writes are not.
    assert!(
        rows.iter()
            .all(|row| matches!(row.property.as_str(), "deferred" | "autostart" | "status"))
    );
    assert_eq!(
        rows.len(),
        5,
        "creation rows for both flags and the status, the deferral and its status move"
    );
    let deferral = rows.iter().rfind(|row| row.property == "deferred").unwrap();
    assert_eq!(deferral.new_value.as_deref(), Some("1"));
    let status = rows.iter().rfind(|row| row.property == "status").unwrap();
    assert_eq!(status.operation_id, deferral.operation_id);
    assert!(deferral.operation_id.is_some());
    let last = deferral;
    assert!(!last.age_is_lower_bound);
    assert!(last.changed_at.parse::<i64>().unwrap() > 0);
    assert!(db.list_task_field_audit("task_missing").unwrap().is_empty());
}

#[test]
fn upgrade_baselines_existing_items_and_starts_recording_deferred() {
    let (_dir, path) = disk_db_path("flag-audit-upgrade");
    let (deferred_id, plain_id) = {
        let db = WorkDb::open(path.clone()).unwrap();
        let product = create_test_product_named(&db, "Flag upgrade");
        let deferred = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product.id.clone())
                    .name("Deferred before upgrade")
                    .deferred(true)
                    .build(),
            )
            .unwrap();
        let plain = create_test_chore_manual(&db, &product.id, "Plain before upgrade");
        // Reconstruct a database that predates the `deferred` ledger.
        let conn = db.connect().unwrap();
        conn.execute_batch(
            "DROP TRIGGER task_status_audit_insert_deferred; DROP TRIGGER task_status_audit_update_deferred;
             DELETE FROM task_status_audit WHERE property = 'deferred';",
        )
        .unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE id = ?1",
            [task_status_audit::DEFERRED_MIGRATION_ID],
        )
        .unwrap();
        (deferred.id, plain.id)
    };
    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        let baseline = |id: &str| {
            db.list_task_field_audit(id)
                .unwrap()
                .into_iter()
                .filter(|row| row.property == "deferred")
                .map(|row| (row.new_value, row.basis, row.age_is_lower_bound))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            baseline(&deferred_id),
            [(Some("1".to_owned()), "unknown_legacy".to_owned(), true)]
        );
        assert_eq!(
            baseline(&plain_id),
            [(Some("0".to_owned()), "unknown_legacy".to_owned(), true)]
        );
    }
    let db = WorkDb::open(path).unwrap();
    db.update_work_item_as_actor(
        &deferred_id,
        WorkItemPatch {
            deferred: Some(false),
            ..Default::default()
        },
        "human",
    )
    .unwrap();
    assert_eq!(
        flag_history(&db, &deferred_id, "deferred").last(),
        Some(&history_row(Some("1"), "0", "human", "update_task"))
    );
}

fn insert_trigger_rows(db: &WorkDb, id: &str) -> Vec<(String, Option<String>, String, String)> {
    let conn = db.connect().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT property, new_value, actor, basis FROM task_status_audit
             WHERE work_item_id = ?1 AND old_value IS NULL
             ORDER BY property",
        )
        .unwrap();
    stmt.query_map([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn production_creates_record_insert_trigger_rows_with_creator_basis() {
    let db = WorkDb::open(temp_db_path("audit-insert-creators")).unwrap();
    let product = create_test_product_named(&db, "Insert creators");
    let project = db
        .create_project(
            CreateProjectInput::builder()
                .product_id(product.id.clone())
                .name("Alpha")
                .build(),
        )
        .unwrap();
    let design_id: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT id FROM tasks WHERE project_id = ?1 AND kind = 'design'",
            [&project.id],
            |row| row.get(0),
        )
        .unwrap();
    let task = db
        .create_task(
            CreateTaskInput::builder()
                .product_id(product.id.clone())
                .project_id(project.id.clone())
                .name("Project task")
                .build(),
        )
        .unwrap();
    let investigation = db
        .create_investigation(
            boss_protocol::CreateInvestigationInput::builder()
                .product_id(product.id.clone())
                .name("Investigate")
                .build(),
        )
        .unwrap();
    let postmortem = db
        .create_design_postmortem(&product.id, &project.id, &project.name, "review merged PRs".to_owned())
        .unwrap();

    let expected_properties = [
        "autostart",
        "blocked_reason",
        "deferred",
        "dispatch_failed_reason",
        "status",
    ];
    for (id, basis) in [
        (design_id.as_str(), "insert_design_task_for_project_in_tx"),
        (task.id.as_str(), "insert_task_in_tx_as_actor"),
        (investigation.id.as_str(), "insert_investigation_in_tx_as_actor"),
        (postmortem.id.as_str(), "insert_design_postmortem_in_tx"),
    ] {
        let rows = insert_trigger_rows(&db, id);
        let properties: Vec<&str> = rows.iter().map(|row| row.0.as_str()).collect();
        assert_eq!(properties, expected_properties, "creator {basis}");
        assert!(
            rows.iter().all(|row| row.2 == "engine" && row.3 == basis),
            "creator {basis} rows: {rows:?}"
        );
        let status = rows.iter().find(|row| row.0 == "status").unwrap();
        assert_eq!(status.1.as_deref(), Some("todo"));
    }
}

#[test]
fn upgrade_refreshes_blocked_reason_trigger_for_repeated_dialog_failures() {
    let (_dir, path) = disk_db_path("dialog-audit-upgrade");
    let task_id = {
        let db = WorkDb::open(path.clone()).unwrap();
        let product = create_test_product_named(&db, "Dialog upgrade");
        let task = create_test_chore_manual(&db, &product.id, "Blocked worker");
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET blocked_reason = 'worker_failed' WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'task_status_audit_update_blocked_reason'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let old_sql = sql.replace("'blocking_dialog_on_screen',", "");
        assert_ne!(sql, old_sql);
        conn.execute_batch("DROP TRIGGER task_status_audit_update_blocked_reason")
            .unwrap();
        conn.execute_batch(&old_sql).unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE id = ?1",
            ["20261114_1800_refresh_blocked_reason_audit_trigger"],
        )
        .unwrap();
        task.id
    };
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'task_status_audit_update_blocked_reason'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for basis in crate::pr_flow::WorkerFailureBasis::ALL {
        assert!(
            sql.contains(&format!("'{}'", basis.as_str())),
            "missing {}",
            basis.as_str()
        );
    }
    let count = || -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'",
            [&task_id],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = count();
    task_status_audit::execute(
        &conn,
        "blocking_dialog_on_screen",
        "engine",
        "UPDATE tasks SET blocked_reason = 'worker_failed' WHERE id = ?1",
        [&task_id],
    )
    .unwrap();
    assert_eq!(count(), before + 1);
    let (old_value, new_value, basis): (String, String, String) = conn.query_row(
        "SELECT old_value, new_value, basis FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason' ORDER BY id DESC LIMIT 1",
        [&task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(old_value, "worker_failed");
    assert_eq!(new_value, "worker_failed");
    assert_eq!(basis, "blocking_dialog_on_screen");
}

#[test]
fn upgrade_refreshes_blocked_reason_trigger_for_repeated_merged_unsupported_base_failures() {
    let (_dir, path) = disk_db_path("merged-base-audit-upgrade");
    let task_id = {
        let db = WorkDb::open(path.clone()).unwrap();
        let product = create_test_product_named(&db, "Merged-base upgrade");
        let task = create_test_chore_manual(&db, &product.id, "Blocked worker");
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET blocked_reason = 'worker_failed' WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'task_status_audit_update_blocked_reason'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let old_sql = sql.replace("'merged_unsupported_base',", "");
        assert_ne!(sql, old_sql);
        conn.execute_batch("DROP TRIGGER task_status_audit_update_blocked_reason")
            .unwrap();
        conn.execute_batch(&old_sql).unwrap();
        conn.execute(
            "DELETE FROM schema_migrations WHERE id = ?1",
            ["20261121_1800_refresh_blocked_reason_audit_trigger_merged_base"],
        )
        .unwrap();
        task.id
    };
    let db = WorkDb::open(path).unwrap();
    let conn = db.connect().unwrap();
    let sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'task_status_audit_update_blocked_reason'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for basis in crate::pr_flow::WorkerFailureBasis::ALL {
        assert!(
            sql.contains(&format!("'{}'", basis.as_str())),
            "missing {}",
            basis.as_str()
        );
    }
    let count = || -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason'",
            [&task_id],
            |row| row.get(0),
        )
        .unwrap()
    };
    let before = count();
    task_status_audit::execute(
        &conn,
        "merged_unsupported_base",
        "engine",
        "UPDATE tasks SET blocked_reason = 'worker_failed' WHERE id = ?1",
        [&task_id],
    )
    .unwrap();
    assert_eq!(count(), before + 1);
    let (old_value, new_value, basis): (String, String, String) = conn.query_row(
        "SELECT old_value, new_value, basis FROM task_status_audit WHERE work_item_id = ?1 AND property = 'blocked_reason' ORDER BY id DESC LIMIT 1",
        [&task_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).unwrap();
    assert_eq!(old_value, "worker_failed");
    assert_eq!(new_value, "worker_failed");
    assert_eq!(basis, "merged_unsupported_base");
}

// ── Execution abandonment history ──────────────────────────────────────────
//
// Every engine write that moves an execution to `abandoned` must leave a
// ledger row with a stable reason code, the previous status, and snapshots
// that stay true after the execution is pruned or the item moves on.

type AbandonRow = boss_protocol::TaskFieldAuditRow;

fn abandonments(db: &WorkDb, work_item_id: &str) -> Vec<AbandonRow> {
    db.list_task_field_audit(work_item_id)
        .unwrap()
        .into_iter()
        .filter(|row| row.property == task_status_audit::EXECUTION_ABANDONED_PROPERTY)
        .collect()
}

fn the_abandonment(db: &WorkDb, work_item_id: &str, code: &str) -> AbandonRow {
    let rows = abandonments(db, work_item_id);
    assert_eq!(rows.len(), 1, "exactly one abandonment expected, got {rows:?}");
    let row = rows.into_iter().next().unwrap();
    assert_eq!(row.reason_code.as_deref(), Some(code));
    assert_eq!(row.new_value.as_deref(), Some("abandoned"));
    assert!(row.execution_id.is_some());
    assert!(row.detail.as_deref().is_some_and(|detail| !detail.is_empty()));
    assert_ne!(row.basis, task_status_audit::UNATTRIBUTED_WRITE);
    row
}

fn execution_with_status(db: &WorkDb, work_item_id: &str, status: ExecutionStatus) -> WorkExecution {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(status)
            .repo_remote_url("git@github.com:foo/bar.git")
            .build(),
    )
    .unwrap()
}

#[test]
fn redundant_duplicate_records_code_previous_status_and_never_ran_snapshots() {
    let db = WorkDb::open(temp_db_path("abandon-redundant")).unwrap();
    let product = create_test_product_named(&db, "Abandon redundant");
    let chore = create_test_chore_manual(&db, &product.id, "Duplicate");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    db.mark_execution_redundant(&execution.id).unwrap();

    let row = the_abandonment(&db, &chore.id, "redundant_duplicate");
    assert_eq!(row.old_value.as_deref(), Some("ready"));
    assert_eq!(row.execution_id.as_deref(), Some(execution.id.as_str()));
    assert_eq!(row.actor, "engine");
    assert_eq!(row.basis, "mark_execution_redundant");
    assert_eq!((row.execution_ever_ran, row.item_ever_ran), (Some(false), Some(false)));
    assert!(row.operation_id.is_some());
    assert!(row.changed_at.parse::<i64>().unwrap() > 0);
}

#[test]
fn ghost_active_heal_abandons_and_demotes_in_one_operation() {
    let db = WorkDb::open(temp_db_path("abandon-ghost")).unwrap();
    let product = create_test_product_named(&db, "Abandon ghost");
    let chore = create_test_chore_manual(&db, &product.id, "Ghost");
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    assert_eq!(db.heal_ghost_active_chores().unwrap().len(), 1);

    let row = the_abandonment(&db, &chore.id, "ghost_active_demoted");
    assert_eq!(row.execution_id.as_deref(), Some(execution.id.as_str()));
    let rows = db.list_task_field_audit(&chore.id).unwrap();
    let demotion = rows
        .iter()
        .find(|r| r.property == "status" && r.old_value.as_deref() == Some("active"))
        .expect("the demotion shares an operation with the abandonment, so it is listed");
    assert_eq!(demotion.new_value.as_deref(), Some("todo"));
    assert_eq!(demotion.operation_id, row.operation_id);
    assert_eq!(demotion.reason_code.as_deref(), Some("heal_ghost_active_chores"));
    assert_eq!(demotion.item_ever_ran, Some(false));
    assert!(
        !rows.iter().any(|r| r.property == "status"
            && r.old_value.as_deref() == Some("todo")
            && r.new_value.as_deref() == Some("active")),
        "the human's earlier todo -> active write is a different operation and is not listed"
    );
}

#[test]
fn stranded_execution_on_a_closed_item_is_attributed_to_the_sweep() {
    let db = WorkDb::open(temp_db_path("abandon-stranded")).unwrap();
    let product = create_test_product_named(&db, "Abandon stranded");
    let chore = create_test_chore_manual(&db, &product.id, "Closed");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::WaitingDependency);
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&chore.id])
        .unwrap();
    assert_eq!(db.abandon_stranded_executions_on_closed_work_items().unwrap().len(), 1);
    let row = the_abandonment(&db, &chore.id, "closed_item_stranded");
    assert_eq!(row.old_value.as_deref(), Some("waiting_dependency"));
    assert_eq!(row.execution_id.as_deref(), Some(execution.id.as_str()));
}

#[test]
fn stale_revision_retirement_abandons_and_moves_to_review_together() {
    let db = WorkDb::open(temp_db_path("abandon-stale-revision")).unwrap();
    let product = create_test_product_named(&db, "Abandon stale revision");
    let chore = create_test_chore_manual(&db, &product.id, "Stale");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    assert!(
        db.retire_stale_revision_before_dispatch(&execution.id, &chore.id)
            .unwrap()
    );
    let row = the_abandonment(&db, &chore.id, "stale_revision_retired");
    let status = db
        .list_task_field_audit(&chore.id)
        .unwrap()
        .into_iter()
        .rfind(|r| r.property == "status")
        .unwrap();
    assert_eq!(status.new_value.as_deref(), Some("in_review"));
    assert_eq!(status.operation_id, row.operation_id);
}

#[test]
fn stale_governing_execution_redispatch_is_attributed_and_knows_it_ran() {
    let db = WorkDb::open(temp_db_path("abandon-redispatch")).unwrap();
    let product = create_test_product_named(&db, "Abandon redispatch");
    let chore = create_test_chore_manual(&db, &product.id, "Redispatch");
    let stale = execution_with_status(&db, &chore.id, ExecutionStatus::WaitingHuman);
    db.request_execution_with_live_check(
        RequestExecutionInput::builder()
            .work_item_id(chore.id.clone())
            .audit_actor("human")
            .build(),
        |_| false,
    )
    .unwrap();
    let row = the_abandonment(&db, &chore.id, "stale_governing_redispatch");
    assert_eq!(row.old_value.as_deref(), Some("waiting_human"));
    assert_eq!(row.execution_id.as_deref(), Some(stale.id.as_str()));
    assert_eq!(row.actor, "human");
}

#[test]
fn deferral_with_autostart_off_is_one_correlated_operation_by_the_callers_actor() {
    let db = WorkDb::open(temp_db_path("abandon-deferral")).unwrap();
    let product = create_test_product_named(&db, "Abandon deferral");
    let chore = create_test_chore(&db, &product.id, "Defer me");
    // An autostart chore may already carry its pending execution.
    let execution = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| execution_with_status(&db, &chore.id, ExecutionStatus::Ready));
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            deferred: Some(true),
            autostart: Some(false),
            ..WorkItemPatch::default()
        },
        "boss",
    )
    .unwrap();
    // An unrelated write in the same second must not share the operation.
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("blocked".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();

    let row = the_abandonment(&db, &chore.id, "deferred");
    assert_eq!(row.actor, "boss");
    assert_eq!(row.old_value.as_deref(), Some(execution.status.as_str()));
    assert_eq!(row.execution_id.as_deref(), Some(execution.id.as_str()));
    let rows = db.list_task_field_audit(&chore.id).unwrap();
    let in_operation = |property: &str| {
        rows.iter()
            .rfind(|r| r.property == property && r.operation_id == row.operation_id)
            .unwrap_or_else(|| panic!("{property} missing from the deferral operation: {rows:?}"))
    };
    assert_eq!(in_operation("deferred").new_value.as_deref(), Some("1"));
    assert_eq!(in_operation("autostart").new_value.as_deref(), Some("0"));
    let unrelated = rows.iter().rfind(|r| r.property == "status").unwrap();
    assert_ne!(unrelated.operation_id, row.operation_id);
    assert!(unrelated.operation_id.is_some());
}

#[test]
fn finishing_a_run_as_abandoned_is_recorded_with_the_run_snapshot() {
    let db = WorkDb::open(temp_db_path("abandon-finish-run")).unwrap();
    let product = create_test_product_named(&db, "Abandon finish");
    let chore = create_test_chore_manual(&db, &product.id, "Finish");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    let (execution, run) = db
        .start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Abandoned)
            .run_status("abandoned")
            .build(),
    )
    .unwrap();
    let row = the_abandonment(&db, &chore.id, "run_finished_abandoned");
    assert_eq!(row.old_value.as_deref(), Some("running"));
    assert_eq!((row.execution_ever_ran, row.item_ever_ran), (Some(true), Some(true)));
}

#[test]
fn finishing_a_run_as_abandoned_parks_worker_failure_in_the_same_operation() {
    let db = WorkDb::open(temp_db_path("abandon-finish-run-park")).unwrap();
    let product = create_test_product_named(&db, "Abandon finish park");
    let chore = create_test_chore_manual(&db, &product.id, "Finish park");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    let (execution, run) = db
        .start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    db.finish_execution_run(
        FinishExecutionRunInput::builder()
            .execution_id(&execution.id)
            .run_id(&run.id)
            .execution_status(ExecutionStatus::Abandoned)
            .run_status("abandoned")
            .error_text("pane refused input")
            .record_last_error(true)
            .worker_failure_basis(WorkerFailureBasis::BlockingDialogOnScreen)
            .build(),
    )
    .unwrap();
    let row = the_abandonment(&db, &chore.id, "run_finished_abandoned");
    let status = db
        .list_task_field_audit(&chore.id)
        .unwrap()
        .into_iter()
        .rfind(|r| r.property == "status" && r.new_value.as_deref() == Some("blocked"))
        .expect("worker-failure park shares an operation with the abandonment");
    assert_eq!(status.operation_id, row.operation_id);
    assert!(row.operation_id.is_some());
}

#[test]
fn dispatch_failure_history_keeps_the_error_text_after_the_stamp_is_cleared() {
    let db = WorkDb::open(temp_db_path("abandon-failure-error")).unwrap();
    let product = create_test_product_named(&db, "Abandon failure error");
    let chore = create_test_chore(&db, &product.id, "Failed start");
    assert!(
        db.bounce_dispatch_failed_to_backlog(&chore.id, "lease_failed", "cube refused the lease")
            .unwrap()
    );
    let task = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.dispatch_failed_reason.as_deref(), Some("lease_failed"));
    assert_eq!(task.dispatch_failed_error.as_deref(), Some("cube refused the lease"));
    let stamp = task
        .dispatch_failed_at
        .clone()
        .expect("bounce stamps dispatch_failed_at");

    let parked = db
        .list_task_field_audit(&chore.id)
        .unwrap()
        .into_iter()
        .rfind(|row| row.property == "autostart" && row.new_value.as_deref() == Some("0"))
        .unwrap();
    assert_eq!(parked.failure_error.as_deref(), Some("cube refused the lease"));
    assert_ne!(parked.failure_error.as_deref(), Some("lease_failed"));
    assert_eq!(parked.failure_stamp.as_deref(), Some(stamp.as_str()));

    let execution = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| execution_with_status(&db, &chore.id, ExecutionStatus::Ready));
    db.mark_execution_redundant(&execution.id).unwrap();
    let abandonment = the_abandonment(&db, &chore.id, "redundant_duplicate");
    assert_eq!(abandonment.failure_error.as_deref(), Some("cube refused the lease"));
    assert_eq!(abandonment.failure_stamp.as_deref(), Some(stamp.as_str()));

    db.request_execution(RequestExecutionInput::builder().work_item_id(chore.id.clone()).build())
        .unwrap();
    let task = match db.get_work_item(&chore.id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task,
        other => panic!("expected chore, got {other:?}"),
    };
    assert!(task.dispatch_failed_reason.is_none());
    assert!(task.dispatch_failed_error.is_none());
    assert!(task.dispatch_failed_at.is_none());

    let after = db.list_task_field_audit(&chore.id).unwrap();
    let abandonment = after
        .iter()
        .find(|row| row.reason_code.as_deref() == Some("redundant_duplicate"))
        .unwrap();
    assert_eq!(abandonment.failure_error.as_deref(), Some("cube refused the lease"));
    assert_eq!(abandonment.failure_stamp.as_deref(), Some(stamp.as_str()));
    let parked = after
        .iter()
        .rfind(|row| row.property == "autostart" && row.new_value.as_deref() == Some("0"))
        .unwrap();
    assert_eq!(parked.failure_error.as_deref(), Some("cube refused the lease"));
    assert_eq!(parked.failure_stamp.as_deref(), Some(stamp.as_str()));
}

#[test]
fn item_ever_ran_matches_execution_ever_ran_when_started_without_a_run_row() {
    let db = WorkDb::open(temp_db_path("abandon-started-no-run")).unwrap();
    let product = create_test_product_named(&db, "Abandon started");
    let chore = create_test_chore_manual(&db, &product.id, "Started");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET started_at = '1700000000' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    db.mark_execution_redundant(&execution.id).unwrap();
    let row = the_abandonment(&db, &chore.id, "redundant_duplicate");
    assert_eq!((row.execution_ever_ran, row.item_ever_ran), (Some(true), Some(true)));
}

#[test]
fn dropping_an_operation_without_end_does_not_correlate_later_writes() {
    let db = WorkDb::open(temp_db_path("abandon-drop-op")).unwrap();
    let product = create_test_product_named(&db, "Abandon drop");
    let chore = create_test_chore_manual(&db, &product.id, "Drop");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    let mut conn = db.connect().unwrap();
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    {
        let _operation = task_status_audit::begin_operation(&tx).unwrap();
        task_status_audit::abandon(
            &tx,
            task_status_audit::abandon_cause::REDUNDANT_DUPLICATE,
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    }
    task_status_audit::execute(
        &tx,
        "unrelated_status",
        "human",
        "UPDATE tasks SET status = 'blocked', last_status_actor = 'human' WHERE id = ?1",
        [&chore.id],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(conn);
    let row = the_abandonment(&db, &chore.id, "redundant_duplicate");
    let later: (Option<String>,) = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT operation_id FROM task_status_audit
             WHERE work_item_id = ?1 AND property = 'status' AND new_value = 'blocked'
             ORDER BY id DESC LIMIT 1",
            [&chore.id],
            |r| Ok((r.get(0)?,)),
        )
        .unwrap();
    assert_ne!(later.0, row.operation_id);
    assert!(row.operation_id.is_some());
    assert!(later.0.is_some());
}

#[test]
fn starting_a_run_records_status_and_autostart_in_one_operation() {
    let db = WorkDb::open(temp_db_path("abandon-run-start-op")).unwrap();
    let product = create_test_product_named(&db, "Run start op");
    let chore = create_test_chore(&db, &product.id, "Start");
    let execution = db
        .list_executions(Some(&chore.id))
        .unwrap()
        .into_iter()
        .next()
        .unwrap_or_else(|| execution_with_status(&db, &chore.id, ExecutionStatus::Ready));
    db.start_execution_run(&execution.id, "worker-1", "mono", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();
    let rows = db.list_task_field_audit(&chore.id).unwrap();
    let autostart = rows
        .iter()
        .rfind(|row| row.property == "autostart" && row.new_value.as_deref() == Some("0"))
        .unwrap();
    let status = rows
        .iter()
        .rfind(|row| row.property == "status" && row.new_value.as_deref() == Some("active"))
        .expect("status move shares an operation with the autostart clear");
    assert_eq!(status.operation_id, autostart.operation_id);
    assert!(autostart.operation_id.is_some());
}

#[test]
fn a_write_that_bypasses_the_helper_is_recorded_as_an_unattributed_abandonment() {
    let db = WorkDb::open(temp_db_path("abandon-unattributed")).unwrap();
    let product = create_test_product_named(&db, "Abandon raw");
    let chore = create_test_chore_manual(&db, &product.id, "Raw");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    let rows = abandonments(&db, &chore.id);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reason_code.as_deref(), Some("unattributed_write"));
    assert_eq!(rows[0].basis, "unattributed_write");
    assert_eq!(rows[0].operation_id, None);
}

#[test]
fn abandon_history_rolls_back_with_its_transaction_and_leaves_no_context_behind() {
    let db = WorkDb::open(temp_db_path("abandon-rollback")).unwrap();
    let product = create_test_product_named(&db, "Abandon rollback");
    let chore = create_test_chore_manual(&db, &product.id, "Rollback");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    let mut conn = db.connect().unwrap();
    {
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let operation = task_status_audit::begin_operation(&tx).unwrap();
        task_status_audit::abandon(
            &tx,
            task_status_audit::abandon_cause::REDUNDANT_DUPLICATE,
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
        operation.end().unwrap();
        // Dropping the transaction rolls the state change and its history back.
    }
    drop(conn);
    assert!(abandonments(&db, &chore.id).is_empty());
    assert_eq!(db.get_execution(&execution.id).unwrap().status, ExecutionStatus::Ready);

    // A statement that fails inside the helper is undone by its savepoint,
    // leaving neither the cause context nor the operation behind.
    let conn = db.connect().unwrap();
    let failed = task_status_audit::abandon(
        &conn,
        task_status_audit::abandon_cause::REDUNDANT_DUPLICATE,
        "UPDATE no_such_table SET status = 'abandoned'",
        [],
    );
    assert!(failed.is_err());
    for table in ["task_status_audit_context", "task_status_audit_operation"] {
        let rows: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "{table} must be empty after a failed write");
    }
}

#[test]
fn abandon_history_survives_later_status_changes_and_pruning_of_the_execution() {
    let db = WorkDb::open(temp_db_path("abandon-retained")).unwrap();
    let product = create_test_product_named(&db, "Abandon retained");
    let chore = create_test_chore_manual(&db, &product.id, "Retained");
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    db.mark_execution_redundant(&execution.id).unwrap();
    let before = the_abandonment(&db, &chore.id, "redundant_duplicate");

    // A later status change, and the execution row disappearing as retention
    // does for a never-started terminal row, leave the history as it was.
    db.update_work_item_as_actor(
        &chore.id,
        WorkItemPatch {
            status: Some("active".to_owned()),
            ..WorkItemPatch::default()
        },
        "human",
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute("DELETE FROM work_executions WHERE id = ?1", [&execution.id])
        .unwrap();
    assert!(db.list_executions(Some(&chore.id)).unwrap().is_empty());
    let after = the_abandonment(&db, &chore.id, "redundant_duplicate");
    assert_eq!(after, before);
}

#[test]
fn upgrading_keeps_old_ledger_rows_unattributed_and_records_new_abandonments() {
    let (_dir, path) = disk_db_path("abandon-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_named(&db, "Abandon upgrade");
    let chore = create_test_chore_manual(&db, &product.id, "Upgrade");
    let conn = db.connect().unwrap();
    // Reconstruct the ledger as it was before this migration: old recorders,
    // none of the added columns, no operation table.
    for property in [
        "status",
        "blocked_reason",
        "dispatch_failed_reason",
        "autostart",
        "deferred",
    ] {
        conn.execute_batch(&format!(
            "DROP TRIGGER task_status_audit_insert_{property}; DROP TRIGGER task_status_audit_update_{property};"
        ))
        .unwrap();
    }
    conn.execute_batch(
        "DROP TRIGGER task_status_audit_abandon_execution;
         DROP INDEX task_status_audit_operation_id;
         DROP TABLE task_status_audit_operation;
         ALTER TABLE task_status_audit DROP COLUMN operation_id;
         ALTER TABLE task_status_audit DROP COLUMN reason_code;
         ALTER TABLE task_status_audit DROP COLUMN detail;
         ALTER TABLE task_status_audit DROP COLUMN execution_id;
         ALTER TABLE task_status_audit DROP COLUMN failure_stamp;
         ALTER TABLE task_status_audit DROP COLUMN failure_error;
         ALTER TABLE task_status_audit DROP COLUMN execution_ever_ran;
         ALTER TABLE task_status_audit DROP COLUMN item_ever_ran;
         ALTER TABLE task_status_audit_context DROP COLUMN reason_code;
         ALTER TABLE task_status_audit_context DROP COLUMN detail;",
    )
    .unwrap();
    for property in [
        "status",
        "blocked_reason",
        "dispatch_failed_reason",
        "autostart",
        "deferred",
    ] {
        task_status_audit::install_property_triggers(&conn, property).unwrap();
    }
    conn.execute("UPDATE tasks SET autostart = 1 WHERE id = ?1", [&chore.id])
        .unwrap();
    conn.execute(
        "DELETE FROM schema_migrations WHERE id = ?1",
        [task_status_audit::EXECUTION_ABANDON_MIGRATION_ID],
    )
    .unwrap();
    drop(conn);
    drop(db);

    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        schema_init::assert_all_migrations_applied(&db.connect().unwrap());
        let old = db
            .list_task_field_audit(&chore.id)
            .unwrap()
            .into_iter()
            .rfind(|r| r.property == "autostart")
            .unwrap();
        assert_eq!(
            old.basis, "unattributed_write",
            "the pre-migration write is kept as recorded"
        );
        assert_eq!(
            (
                &old.reason_code,
                &old.operation_id,
                &old.item_ever_ran,
                &old.execution_id
            ),
            (&None, &None, &None, &None),
            "nothing is invented for a transition recorded before causes were captured"
        );
    }
    let db = WorkDb::open(path).unwrap();
    let execution = execution_with_status(&db, &chore.id, ExecutionStatus::Ready);
    db.mark_execution_redundant(&execution.id).unwrap();
    the_abandonment(&db, &chore.id, "redundant_duplicate");
}
