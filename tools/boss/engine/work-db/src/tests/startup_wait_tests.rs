use super::*;

#[test]
fn failed_and_uncertain_leases_clear_startup_phases_even_after_the_deadline() {
    let db = WorkDb::open(temp_db_path("startup-wait-retry")).unwrap();
    let product = create_test_product(&db);
    for failure in [false, true] {
        let task = create_test_chore(
            &db,
            &product.id,
            if failure { "Failed lease" } else { "Uncertain lease" },
        );
        db.reconcile_product_executions(&product.id).unwrap();
        let execution = db.list_executions(Some(&task.id)).unwrap().remove(0);
        db.claim_execution_for_dispatch(&execution.id).unwrap();
        db.set_dispatch_wait_reason(&execution.id, "leasing_workspace").unwrap();
        let delay = std::time::Duration::from_secs(3600);
        let expected = if failure {
            db.record_pre_start_failure(&execution.id, "worker", None, "lease failed", &[delay])
                .unwrap();
            "failure_backoff"
        } else {
            assert!(
                db.park_pre_start_attempt(&execution.id, delay, "host has not replied")
                    .unwrap()
            );
            "host_lease_park"
        };
        assert_eq!(
            db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason.as_deref(),
            Some(expected)
        );
        let retry = db.get_execution(&execution.id).unwrap();
        assert!(retry.dispatch_wait_reason.is_none());
        assert!(retry.dispatch_wait_since.is_none());
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET dispatch_not_before = '1' WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        assert_eq!(
            db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason.as_deref(),
            Some("awaiting_dispatch")
        );
    }
}

#[test]
fn review_followup_is_queued_before_it_becomes_visible_and_start_consumes_autostart() {
    let db = WorkDb::open(temp_db_path("followup-startup-wait")).unwrap();
    let product = create_test_product(&db);
    let origin = create_test_chore_manual(&db, &product.id, "Merged origin");
    db.update_work_item(
        &origin.id,
        WorkItemPatch {
            pr_url: Some("https://github.com/example/repo/pull/42".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let input = || {
        crate::ReviewFindingsFollowupInsert::builder()
            .product_id(&product.id)
            .name("Review findings")
            .created_via("pr_review:post_merge:startup")
            .description("Address the review findings")
            .chain_root_id(&origin.id)
            .build()
    };
    let task = db.create_review_findings_followup(input()).unwrap();
    assert!(task.autostart);
    assert_eq!(task.kind, TaskKind::Followup);
    let runtime = db.get_task_runtime(&task.id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Ready));
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some("awaiting_dispatch"));
    let execution = runtime.execution_id.unwrap();
    assert_eq!(db.create_review_findings_followup(input()).unwrap().id, task.id);
    assert_eq!(db.list_executions(Some(&task.id)).unwrap().len(), 1);

    db.set_dispatch_wait_reason(&execution, "pool_exhausted").unwrap();
    db.claim_execution_for_dispatch(&execution).unwrap();
    let tree = db.get_work_tree(&product.id).unwrap();
    let runtime = tree
        .task_runtimes
        .iter()
        .find(|runtime| runtime.work_item_id == task.id)
        .unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Claimed));
    assert_eq!(runtime.dispatch_wait_reason.as_deref(), Some("dispatching"));
    assert!(
        runtime.dispatch_wait_since.is_none(),
        "the old pool hold's age is not a startup timestamp"
    );
    assert!(runtime.dispatch_wait_blocker.is_none());
    db.set_dispatch_wait_reason(&execution, "leasing_workspace").unwrap();
    assert!(db.release_dispatch_claim(&execution).unwrap());
    assert_eq!(
        db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason.as_deref(),
        Some("awaiting_dispatch")
    );
    db.claim_execution_for_dispatch(&execution).unwrap();
    db.set_dispatch_wait_reason(&execution, "preparing_workspace").unwrap();
    db.release_stale_claimed_executions().unwrap();
    assert_eq!(
        db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason.as_deref(),
        Some("awaiting_dispatch")
    );
    db.claim_execution_for_dispatch(&execution).unwrap();
    db.start_execution_run(&execution, "worker", "repo", "lease", "workspace", "/tmp/workspace")
        .unwrap();
    let runtime = db.get_task_runtime(&task.id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::Running));
    assert!(runtime.dispatch_wait_reason.is_none());
    assert!(db.get_execution(&execution).unwrap().dispatch_wait_reason.is_none());
    let WorkItem::Chore(started) = db.get_work_item(&task.id).unwrap() else {
        panic!("followup")
    };
    assert_eq!(started.status, TaskStatus::Active);
    assert!(
        !started.autostart,
        "start consumes the single-shot flag; it did not prevent dispatch"
    );

    let manual = db
        .create_review_findings_followup(
            crate::ReviewFindingsFollowupInsert::builder()
                .product_id(&product.id)
                .name("Manual findings")
                .created_via("pr_review:post_merge:manual")
                .description("Wait for an explicit start")
                .chain_root_id(&origin.id)
                .autostart(false)
                .build(),
        )
        .unwrap();
    assert!(db.list_executions(Some(&manual.id)).unwrap().is_empty());
}

#[test]
fn unexplained_wait_is_logged_on_read_without_waiting_for_a_heartbeat() {
    let logs = crate::test_support::log_capture::install();
    let db = WorkDb::open(temp_db_path("unexplained-startup-log")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, &product.id, "Unexplained wait");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&task.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Queued)
                .build(),
        )
        .unwrap();
    assert_eq!(
        db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason.as_deref(),
        Some("unclassified")
    );
    let captured = String::from_utf8(logs.lock().clone()).unwrap();
    let record = captured
        .lines()
        .find(|line| line.contains(&execution.id) && line.contains("exposed waiting state without a reason"))
        .expect("immediate structured anomaly");
    for field in [
        &task.id,
        "execution_status=\"queued\"",
        "task_state_age_secs=",
        "task_state_changed_at=",
        "dispatch_wait_reason",
        "dispatch_not_before",
        "autostart",
    ] {
        assert!(record.contains(field), "missing {field}: {record}");
    }
}

fn anomaly_records(logs: &crate::test_support::log_capture::SharedBuffer, needle: &str) -> Vec<String> {
    String::from_utf8(logs.lock().clone())
        .unwrap()
        .lines()
        .filter(|line| line.contains(needle) && line.contains("exposed waiting state without a reason"))
        .map(str::to_owned)
        .collect()
}

#[test]
fn unexplained_wait_is_logged_once_per_distinct_anomaly() {
    let logs = crate::test_support::log_capture::install();
    let db = WorkDb::open(temp_db_path("unexplained-log-dedup")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, &product.id, "Persistent unexplained wait");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&task.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Queued)
                .build(),
        )
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason.as_deref(),
            Some("unclassified")
        );
    }
    assert_eq!(anomaly_records(&logs, &execution.id).len(), 1);
}

#[test]
fn unexplained_wait_age_follows_the_execution_transition_not_the_task_status() {
    let logs = crate::test_support::log_capture::install();
    let db = WorkDb::open(temp_db_path("unexplained-log-exec-age")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, &product.id, "Execution transition age");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&task.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Queued)
                .build(),
        )
        .unwrap();
    // `connect` holds the database's only connection until it is dropped, so
    // each statement takes its own.
    let age_of = || -> i64 {
        let changed_at: i64 = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT changed_at FROM execution_state_transitions WHERE execution_id = ?1",
                [&execution.id],
                |row| row.get(0),
            )
            .unwrap();
        boss_engine_utils::epoch_time::now_epoch_secs() - changed_at
    };
    // The task's own status never moves, and the wait has no recorded start.
    assert!(execution.dispatch_wait_since.is_none());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE execution_state_transitions SET changed_at = changed_at - 7200 WHERE execution_id = ?1",
            [&execution.id],
        )
        .unwrap();
    assert!(age_of() >= 7200);
    db.get_task_runtime(&task.id).unwrap();
    let record = anomaly_records(&logs, &execution.id).remove(0);
    let age = record
        .split("execution_state_age_secs=")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse::<i64>().ok())
        .unwrap_or_else(|| panic!("no execution age: {record}"));
    assert!((7200..7260).contains(&age), "age {age}: {record}");
    // An execution-level change restarts the clock without touching the task.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_wait_reason = 'leasing_workspace' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    assert!(age_of() < 60);
    let after = crate::query_task(&db.connect().unwrap(), &task.id).unwrap().unwrap();
    assert_eq!(after.status, task.status, "the task status did not move");
}

#[test]
fn unexplained_wait_log_uses_the_given_execution_never_the_newest_row() {
    let logs = crate::test_support::log_capture::install();
    let db = WorkDb::open(temp_db_path("unexplained-log-selection")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, &product.id, "Shadowed execution");
    let selected = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&task.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Queued)
                .build(),
        )
        .unwrap();
    let progression = db
        .create_execution(
            CreateExecutionInput::builder()
                .created_via("test")
                .work_item_id(&task.id)
                .kind(ExecutionKind::Progression)
                .status(ExecutionStatus::Queued)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = '9999999999' WHERE id = ?1",
            [&progression.id],
        )
        .unwrap();
    let conn = db.connect().unwrap();
    crate::wait_diagnostics::log_unclassified(
        &conn,
        &[crate::wait_diagnostics::Unclassified {
            work_item_id: &task.id,
            execution_id: Some(&selected.id),
            execution_status: Some("queued"),
        }],
    );
    assert_eq!(anomaly_records(&logs, &selected.id).len(), 1);
    assert!(anomaly_records(&logs, &progression.id).is_empty());
    crate::wait_diagnostics::log_unclassified(
        &conn,
        &[crate::wait_diagnostics::Unclassified {
            work_item_id: &task.id,
            execution_id: None,
            execution_status: None,
        }],
    );
    assert!(anomaly_records(&logs, &progression.id).is_empty());
    assert!(
        anomaly_records(&logs, "execution_id=\"(none)\"")
            .iter()
            .any(|line| line.contains(&task.id))
    );
}

#[test]
fn heartbeat_and_card_log_the_same_non_abandoned_execution() {
    let logs = crate::test_support::log_capture::install();
    let db = WorkDb::open(temp_db_path("unexplained-log-abandoned")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, &product.id, "Abandoned duplicate");
    let mut ids = Vec::new();
    for (status, kind) in [
        (ExecutionStatus::Completed, ExecutionKind::ChoreImplementation),
        (ExecutionStatus::Abandoned, ExecutionKind::ChoreImplementation),
        (ExecutionStatus::Completed, ExecutionKind::Progression),
    ] {
        ids.push(
            db.create_execution(
                CreateExecutionInput::builder()
                    .created_via("test")
                    .work_item_id(&task.id)
                    .kind(kind)
                    .status(status)
                    .build(),
            )
            .unwrap()
            .id,
        );
    }
    {
        let conn = db.connect().unwrap();
        for (offset, id) in ids.iter().enumerate() {
            conn.execute(
                "UPDATE work_executions SET created_at = ?2 WHERE id = ?1",
                rusqlite::params![id, format!("{}", 1_000 + offset)],
            )
            .unwrap();
        }
        conn.execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&task.id])
            .unwrap();
    }
    let runtime = db.get_task_runtime(&task.id).unwrap();
    assert_eq!(
        runtime.execution_id.as_deref(),
        Some(ids[0].as_str()),
        "card skips the abandoned duplicate"
    );
    db.list_unexplained_dispatch_waits().unwrap();
    for shadow in [&ids[1], &ids[2]] {
        assert!(
            anomaly_records(&logs, shadow).is_empty(),
            "logged a shadowing row {shadow}"
        );
    }
    let logged = anomaly_records(&logs, &task.id);
    assert!(!logged.is_empty(), "the heartbeat should report this item");
    assert!(
        logged
            .iter()
            .all(|line| line.contains(&format!("execution_id=\"{}\"", ids[0]))),
        "{logged:?}"
    );
}

#[test]
fn persistent_anomaly_is_reported_again_after_the_ttl_despite_reads_in_between() {
    let key = "ttl-test-item";
    assert!(crate::wait_diagnostics::report_due_for_test(key, 1_000));
    for now in [1_100, 2_000, 3_000, 4_000] {
        assert!(
            !crate::wait_diagnostics::report_due_for_test(key, now),
            "suppressed at {now}"
        );
    }
    assert!(crate::wait_diagnostics::report_due_for_test(key, 4_600));
    assert!(!crate::wait_diagnostics::report_due_for_test(key, 4_700));
}

#[test]
fn stale_startup_phase_on_a_ready_row_is_not_rendered_as_a_hold() {
    let db = WorkDb::open(temp_db_path("stale-startup-phase")).unwrap();
    let product = create_test_product(&db);
    let task = create_test_chore(&db, &product.id, "Stale phase");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&task.id)).unwrap().remove(0);
    db.claim_execution_for_dispatch(&execution.id).unwrap();
    db.set_dispatch_wait_reason(&execution.id, "leasing_workspace").unwrap();
    // A path that moves the row off `claimed` without clearing the phase.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'ready' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    let reason = db.get_task_runtime(&task.id).unwrap().dispatch_wait_reason;
    assert_ne!(reason.as_deref(), Some("leasing_workspace"));
}
