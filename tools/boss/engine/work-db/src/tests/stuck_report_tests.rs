use super::*;
use boss_protocol::StuckBucket;

fn now() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_secs()
}

// Deterministic process observation; no OS liveness probe.
fn observed_live_pid(pid: Option<i64>) -> bool {
    pid == Some(i64::from(std::process::id()))
}

fn row(db: &WorkDb, id: &str) -> boss_protocol::StuckRow {
    db.stuck_report(0, now(), &observed_live_pid)
        .unwrap()
        .rows
        .into_iter()
        .find(|r| r.work_item_id == id)
        .unwrap()
}

fn question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Change the timeout?".into(),
        explanation: "The test needs a longer timeout.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    }
}

#[test]
fn missing_ledger_uses_task_age_and_does_not_fail_the_report() {
    let db = WorkDb::open(temp_db_path("stuck-missing-ledger")).unwrap();
    let product = create_test_product_named(&db, "Legacy");
    let task = create_test_chore_manual(&db, &product.id, "Missing history");
    let intact = create_test_chore_manual(&db, &product.id, "Intact history");
    let conn = db.connect().unwrap();
    let timestamp = now();
    conn.execute("DELETE FROM task_status_audit WHERE work_item_id = ?1", [&task.id])
        .unwrap();
    conn.execute(
        "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
        params![task.id, timestamp - 100],
    )
    .unwrap();
    drop(conn);
    let report = db.stuck_report(0, timestamp, &observed_live_pid).unwrap();
    let entry = report.rows.iter().find(|r| r.work_item_id == task.id).unwrap();
    assert_eq!(entry.last_ledger_basis, "unknown_legacy");
    assert_eq!(entry.age_in_state_secs, 100);
    assert!(entry.age_is_lower_bound);
    // One item without a ledger row must not hide the rest of the report.
    let other = report.rows.iter().find(|r| r.work_item_id == intact.id).unwrap();
    assert_ne!(other.last_ledger_basis, "unknown_legacy");
}

#[test]
fn project_prerequisite_is_progressing_until_terminal_or_missing() {
    let db = WorkDb::open(temp_db_path("stuck-project-prerequisite")).unwrap();
    let product = create_test_product_named(&db, "Projects");
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Live prerequisite".into(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let task = create_test_chore_manual(&db, &product.id, "Waiting for project");
    db.add_dependency(AddDependencyInput {
        prerequisite: project.id.clone(),
        dependent: task.id.clone(),
        relation: None,
    })
    .unwrap();
    assert_eq!(row(&db, &task.id).bucket, StuckBucket::WaitingOnDependency);
    db.connect()
        .unwrap()
        .execute("UPDATE projects SET status = 'active' WHERE id = ?1", [&project.id])
        .unwrap();
    assert_eq!(row(&db, &task.id).bucket, StuckBucket::WaitingOnDependency);
    for status in ["done", "archived"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE projects SET status = ?2 WHERE id = ?1",
                params![project.id, status],
            )
            .unwrap();
        assert_eq!(row(&db, &task.id).bucket, StuckBucket::EngineStuck);
    }
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_item_dependencies SET prerequisite_id = 'proj_missing' WHERE dependent_id = ?1",
            [&task.id],
        )
        .unwrap();
    assert_eq!(row(&db, &task.id).bucket, StuckBucket::EngineStuck);
}

#[test]
fn report_fixtures_cover_human_capacity_dependency_and_engine_stuck() {
    let db = WorkDb::open(temp_db_path("stuck-buckets")).unwrap();
    let (product, failed, execution) = make_waiting_human_chore(&db, "Buckets");
    db.record_worker_failure_with_basis(&execution, "failed", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    let manual = db
        .create_chore_as_actor(
            CreateChoreInput::builder()
                .product_id(&product)
                .name("Manual")
                .autostart(false)
                .build(),
            "human",
        )
        .unwrap();
    assert_eq!(row(&db, &manual.id).bucket, StuckBucket::WaitingOnHuman);
    assert_eq!(row(&db, &manual.id).classification, "S21_manual_start");
    let capacity = db
        .create_chore_as_actor(
            CreateChoreInput::builder()
                .product_id(&product)
                .name("Capacity")
                .autostart(false)
                .build(),
            "human",
        )
        .unwrap();
    let queued = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&capacity.id).build())
        .unwrap();
    db.set_dispatch_wait_reason(&queued.id, "Waiting for an available worker (8 working)")
        .unwrap();
    assert_eq!(row(&db, &capacity.id).bucket, StuckBucket::WaitingOnCapacity);
    assert_eq!(
        row(&db, &capacity.id).classification,
        "S20_dispatch_wait:Waiting for an available worker (8 working)"
    );
    crate::pr_flow::insert_question(&db.connect().unwrap(), &queued.id, "proposal", &question()).unwrap();
    assert_eq!(row(&db, &capacity.id).bucket, StuckBucket::WaitingOnHuman);
    let dependent = create_test_chore_manual(&db, &product, "Dependent");
    db.add_dependency(AddDependencyInput {
        prerequisite: failed.clone(),
        dependent: dependent.id.clone(),
        relation: None,
    })
    .unwrap();
    assert_eq!(row(&db, &dependent.id).bucket, StuckBucket::WaitingOnDependency);
    assert_eq!(row(&db, &failed).bucket, StuckBucket::EngineStuck);
    assert_eq!(row(&db, &failed).classification, "S7_worker_failed:worker_failed");
    assert_eq!(row(&db, &failed).failure_basis.as_deref(), Some("declared_blocked"));
    let review = create_test_chore_manual(&db, &product, "Behind PR");
    db.update_task(
        &review.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        },
        "human",
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_merge_state_status = 'BEHIND' WHERE id = ?1",
            [&review.id],
        )
        .unwrap();
    assert_eq!(row(&db, &review.id).bucket, StuckBucket::EngineStuck);
    assert_eq!(row(&db, &review.id).classification, "S18_pr_behind");
    assert_eq!(
        db.stuck_report(0, now(), &observed_live_pid)
            .unwrap()
            .unconverted_declared_blockers,
        1
    );
}

#[test]
fn minimum_age_filters_young_rows_and_negative_age_is_rejected() {
    let db = WorkDb::open(temp_db_path("stuck-min-age")).unwrap();
    let product = create_test_product_named(&db, "Ages");
    let task = create_test_chore_manual(&db, &product.id, "Fresh");
    assert!(
        db.stuck_report(0, now(), &observed_live_pid)
            .unwrap()
            .rows
            .iter()
            .any(|r| r.work_item_id == task.id)
    );
    assert!(
        !db.stuck_report(604800, now(), &observed_live_pid)
            .unwrap()
            .rows
            .iter()
            .any(|r| r.work_item_id == task.id)
    );
    assert!(db.stuck_report(-1, now(), &observed_live_pid).is_err());
}

#[test]
fn questions_keep_their_filed_classification_after_answer_and_state_changes() {
    let db = WorkDb::open(temp_db_path("stuck-question-counts")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Question counts");
    let q = question();
    crate::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "decision", &q)
        .unwrap();
    assert_eq!(row(&db, &id).bucket, StuckBucket::WaitingOnHuman);
    db.answer_operator_question(&id, boss_protocol::OperatorAnswer::YesNo { value: false })
        .unwrap();
    let report = db.stuck_report(604800, now(), &observed_live_pid).unwrap();
    assert_eq!(report.questions_filed, 1);
    assert_eq!(report.questions_filed_by_stuck_kind["S8_operator_question"], 1);
    assert_eq!(report.unconverted_declared_blockers, 0);
}

#[test]
fn waiting_human_age_tracks_execution_transition_and_live_work_is_excluded() {
    let db = WorkDb::open(temp_db_path("stuck-human-transition")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Human transition");
    let timestamp = now();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE task_status_audit SET changed_at = ?1 WHERE property = 'execution_waiting_human'",
            [timestamp - 100],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET updated_at = ?1 WHERE id = ?2", params![timestamp, id])
        .unwrap();
    let report = db.stuck_report(50, timestamp, &observed_live_pid).unwrap();
    let entry = report.rows.iter().find(|r| r.work_item_id == id).unwrap();
    assert_eq!(entry.classification, "S19_waiting_human");
    assert_eq!(entry.bucket, StuckBucket::WaitingOnHuman);
    assert_eq!(entry.age_in_state_secs, 100);
    assert!(!entry.age_is_lower_bound);
    let abandoned = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Abandoned)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET created_at = ?1 WHERE id = ?2",
            params![timestamp + 1, abandoned.id],
        )
        .unwrap();
    assert_eq!(row(&db, &id).classification, "S19_waiting_human");
    assert_eq!(row(&db, &id).bucket, StuckBucket::WaitingOnHuman);
    for status in ["claimed", "running"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?1 WHERE id = ?2",
                params![status, execution],
            )
            .unwrap();
        assert!(
            !db.stuck_report(0, timestamp, &observed_live_pid)
                .unwrap()
                .rows
                .iter()
                .any(|r| r.work_item_id == id)
        );
    }
    db.set_execution_awaiting_human(&execution, true).unwrap();
    assert!(row(&db, &id).age_in_state_secs < 50);
    assert_eq!(
        db.connect()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM task_status_audit WHERE property = 'execution_waiting_human'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
}

#[test]
fn waiting_human_migration_backfills_lower_bound_and_survives_reopen() {
    let (_dir, path) = disk_db_path("stuck-wait-upgrade");
    let db = WorkDb::open(path.clone()).unwrap();
    let (_, id, _) = make_waiting_human_chore(&db, "Historical wait");
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "DROP TRIGGER execution_waiting_human_insert;
        DROP TRIGGER execution_waiting_human_update;
        DELETE FROM task_status_audit WHERE property = 'execution_waiting_human';
        DELETE FROM schema_migrations WHERE id = '20261029_1900_execution_waiting_human_history';",
    )
    .unwrap();
    drop(conn);
    drop(db);
    for _ in 0..2 {
        let db = WorkDb::open(path.clone()).unwrap();
        let entry = row(&db, &id);
        assert_eq!(entry.classification, "S19_waiting_human");
        assert!(entry.age_is_lower_bound);
        assert_eq!(
            db.connect()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM task_status_audit WHERE property = 'execution_waiting_human'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }
}

#[test]
fn liveness_observer_runs_after_the_connection_lock_is_released() {
    let db = WorkDb::open(temp_db_path("stuck-observer-lock")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Observer lock");
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
        [&execution],
    )
    .unwrap();
    conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&id])
        .unwrap();
    conn.execute(
        "UPDATE work_runs SET shell_pid = ?1, host_id = 'local' WHERE execution_id = ?2",
        params![std::process::id(), execution],
    )
    .unwrap();
    drop(conn);
    let calls = std::cell::Cell::new(0);
    // Re-acquiring the non-reentrant connection lock would deadlock if the
    // observer ran while `stuck_report` still held it.
    let report = db
        .stuck_report(0, now(), &|pid| {
            calls.set(calls.get() + 1);
            db.connect().unwrap();
            pid == Some(i64::from(std::process::id()))
        })
        .unwrap();
    assert_eq!(calls.get(), 1);
    let entry = report.rows.iter().find(|r| r.work_item_id == id).unwrap();
    assert_eq!(entry.classification, "S15_orphan_live_pid");
}

#[test]
fn orphan_with_durable_live_pid_is_distinguished_from_missing_pid() {
    let db = WorkDb::open(temp_db_path("stuck-orphan-pid")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Orphan pid");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&id])
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET shell_pid = ?1, host_id = 'local' WHERE execution_id = ?2",
            params![std::process::id(), execution],
        )
        .unwrap();
    assert_eq!(row(&db, &id).classification, "S15_orphan_live_pid");
    let report = db
        .stuck_report(0, now(), &|pid| {
            assert_eq!(pid, Some(i64::from(std::process::id())));
            false
        })
        .unwrap();
    assert_eq!(
        report
            .rows
            .iter()
            .find(|row| row.work_item_id == id)
            .unwrap()
            .classification,
        "orphan_active"
    );
    let timestamp = 1_000_000;
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET created_at = ?1 WHERE execution_id = ?2",
            params![timestamp, execution],
        )
        .unwrap();
    for (report_time, expected) in [
        (timestamp, "S15_orphan_live_pid"),
        (timestamp + crate::REDISPATCH_PID_TRUST_SECS, "S15_orphan_live_pid"),
        (timestamp + crate::REDISPATCH_PID_TRUST_SECS + 1, "orphan_active"),
    ] {
        let report = db.stuck_report(0, report_time, &observed_live_pid).unwrap();
        assert_eq!(
            report
                .rows
                .iter()
                .find(|r| r.work_item_id == id)
                .unwrap()
                .classification,
            expected
        );
        assert_eq!(
            db.latest_local_worker_process_for_work_item(&id, crate::REDISPATCH_PID_TRUST_SECS, report_time,)
                .unwrap()
                .is_some(),
            expected == "S15_orphan_live_pid"
        );
    }
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_runs SET shell_pid = NULL WHERE execution_id = ?1",
            [&execution],
        )
        .unwrap();
    assert_eq!(row(&db, &id).classification, "orphan_active");
}

#[test]
fn human_only_gates_do_not_count_as_progressing_prerequisites() {
    let db = WorkDb::open(temp_db_path("stuck-human-gates")).unwrap();
    let product = create_test_product_named(&db, "Human gates");
    for (status, reason, human_driven, actor, expected) in [
        (
            "blocked",
            Some("deletion_signoff"),
            false,
            "engine",
            StuckBucket::WaitingOnHuman,
        ),
        ("blocked", None, false, "human", StuckBucket::WaitingOnHuman),
        ("blocked", None, false, "boss", StuckBucket::WaitingOnHuman),
        ("blocked", None, false, "boothby", StuckBucket::WaitingOnHuman),
        ("blocked", None, false, "progression", StuckBucket::WaitingOnHuman),
        ("todo", None, true, "engine", StuckBucket::WaitingOnHuman),
        ("blocked", None, false, "engine", StuckBucket::EngineStuck),
    ] {
        let task = create_test_chore_manual(&db, &product.id, format!("Gate {status} {reason:?} {actor}"));
        db.connect().unwrap().execute("UPDATE tasks SET status = ?1, blocked_reason = ?2, human_driven = ?3, last_status_actor = ?4 WHERE id = ?5", params![status, reason, human_driven, actor, task.id]).unwrap();
        assert_eq!(row(&db, &task.id).bucket, expected);
        db.connect()
            .unwrap()
            .execute(
                "DELETE FROM task_status_audit WHERE work_item_id = ?1 AND property = 'status'",
                [&task.id],
            )
            .unwrap();
        assert_eq!(row(&db, &task.id).bucket, expected);
        let dependent = create_test_chore_manual(&db, &product.id, format!("Dependent on {}", task.name));
        db.add_dependency(AddDependencyInput {
            prerequisite: task.id.clone(),
            dependent: dependent.id.clone(),
            relation: None,
        })
        .unwrap();
        assert_eq!(
            row(&db, &dependent.id).bucket,
            if expected == StuckBucket::WaitingOnHuman {
                StuckBucket::EngineStuck
            } else {
                StuckBucket::WaitingOnDependency
            }
        );
    }
}

#[test]
fn declared_blocker_count_includes_park_and_nudge_but_excludes_operator_no() {
    let db = WorkDb::open(temp_db_path("stuck-declared-count")).unwrap();
    for (basis, expected) in [
        (WorkerFailureBasis::DeclaredBlocked, 1),
        (WorkerFailureBasis::NudgeBreaker, 2),
        (WorkerFailureBasis::OperatorNo, 2),
    ] {
        let (_, _, execution) = make_waiting_human_chore(&db, basis.as_str());
        db.record_worker_failure_with_basis(&execution, "blocked", basis)
            .unwrap();
        assert_eq!(
            db.stuck_report(604800, now(), &observed_live_pid)
                .unwrap()
                .unconverted_declared_blockers,
            expected
        );
    }
    let product = create_test_product_named(&db, "Park");
    let task = create_test_chore_manual(&db, &product.id, "Park");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET dispatch_failed_reason = 'deliberate_park' WHERE id = ?1",
            [&task.id],
        )
        .unwrap();
    assert_eq!(
        db.stuck_report(604800, now(), &observed_live_pid)
            .unwrap()
            .unconverted_declared_blockers,
        3
    );
}

#[test]
fn classification_table_covers_failure_attention_and_global_hold_precedence() {
    let db = WorkDb::open(temp_db_path("stuck-classifications")).unwrap();
    let product = create_test_product_named(&db, "Classifications");
    let root = create_test_chore_manual(&db, &product.id, "Green parent");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET ci_required_state = 'pass' WHERE id = ?1", [&root.id])
        .unwrap();
    for (dispatch, blocked, status, attention, ci, revision, wait, expected) in [
        (
            Some("spawn_failed"),
            None,
            "todo",
            None,
            None,
            false,
            None,
            "S1_dispatch_failure:spawn_failed",
        ),
        (
            Some("churn_guard"),
            None,
            "todo",
            None,
            None,
            false,
            None,
            "S2_churn_guard",
        ),
        (
            Some("deliberate_park"),
            None,
            "todo",
            None,
            None,
            false,
            None,
            "S3_deliberate_park",
        ),
        (
            None,
            None,
            "in_review",
            None,
            Some("fail"),
            false,
            None,
            "S6_ci_failure",
        ),
        (
            None,
            Some("worker_failed"),
            "blocked",
            None,
            None,
            true,
            None,
            "S13_revision_chain:worker_failed",
        ),
        (
            None,
            None,
            "active",
            Some("worker_recovery_exhausted"),
            None,
            false,
            None,
            "S14_worker_recovery",
        ),
        (
            None,
            None,
            "in_review",
            Some("churn_guard_parked"),
            None,
            false,
            None,
            "S16_review_churn",
        ),
        (
            None,
            None,
            "in_review",
            Some("trunk_queue_failed"),
            Some("fail"),
            false,
            None,
            "S17_merge_queue",
        ),
        (
            None,
            None,
            "active",
            Some("abandoned_branch_no_pr"),
            None,
            false,
            None,
            "S22_abandoned_branch",
        ),
        (
            None,
            None,
            "todo",
            None,
            None,
            false,
            Some("dispatch_paused"),
            "S23_global_hold:dispatch_paused",
        ),
        (
            None,
            None,
            "todo",
            None,
            None,
            false,
            Some("automation_paused"),
            "S23_global_hold:automation_paused",
        ),
    ] {
        let task = create_test_chore_manual(&db, &product.id, expected);
        db.connect().unwrap().execute("UPDATE tasks SET dispatch_failed_reason = ?1, blocked_reason = ?2, status = ?3, ci_required_state = ?4, kind = ?5, parent_task_id = ?6 WHERE id = ?7", params![dispatch, blocked, status, ci, if revision { "revision" } else { "chore" }, if revision { Some(&root.id) } else { None }, task.id]).unwrap();
        if let Some(kind) = attention {
            db.connect().unwrap().execute("INSERT INTO work_attention_items (id, work_item_id, kind, status, title, body_markdown, created_at) VALUES (?1, ?1, ?2, 'open', 'Attention', '', '0')", params![task.id, kind]).unwrap();
        }
        if let Some(wait) = wait {
            let execution = db
                .request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
                .unwrap();
            db.set_dispatch_wait_reason(&execution.id, wait).unwrap();
            assert_eq!(row(&db, &task.id).bucket, StuckBucket::WaitingOnCapacity);
        }
        assert_eq!(row(&db, &task.id).classification, expected);
    }
}

#[test]
fn ended_human_wait_does_not_replace_task_age_or_basis() {
    let db = WorkDb::open(temp_db_path("stuck-ended-wait")).unwrap();
    let timestamp = now();
    for (status, legacy) in [("active", false), ("in_review", true)] {
        let (_, id, execution) = make_waiting_human_chore(&db, status);
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = ?1 WHERE id = ?2", params![status, id])
            .unwrap();
        conn.execute("UPDATE task_status_audit SET changed_at = ?1, basis = 'status_change', age_is_lower_bound = 0 WHERE work_item_id = ?2 AND property != 'execution_waiting_human'", params![timestamp - 500, id]).unwrap();
        // Resuming and waiting again places the wait after the last task transition.
        conn.execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'waiting_human' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
        conn.execute("UPDATE task_status_audit SET changed_at = ?1, basis = ?2, age_is_lower_bound = ?3 WHERE work_item_id = ?4 AND property = 'execution_waiting_human'", params![timestamp - 100, if legacy { "unknown_legacy" } else { "execution_waiting_human" }, legacy, id]).unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&execution],
        )
        .unwrap();
        drop(conn);
        let report = db.stuck_report(200, timestamp, &observed_live_pid).unwrap();
        let entry = report.rows.iter().find(|r| r.work_item_id == id).unwrap();
        assert_eq!(entry.bucket, StuckBucket::EngineStuck);
        assert_eq!(entry.age_in_state_secs, 500);
        assert_eq!(entry.last_ledger_basis, "status_change");
        assert!(!entry.age_is_lower_bound);
    }
}

#[test]
fn non_task_waits_never_enter_task_history() {
    let db = WorkDb::open(temp_db_path("stuck-non-task-wait")).unwrap();
    let product = create_test_product_named(&db, "Design waits");
    let project = db
        .create_project(CreateProjectInput {
            product_id: product.id.clone(),
            name: "Design project".into(),
            description: None,
            goal: None,
            autostart: false,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    for (id, kind) in [
        (&product.id, ExecutionKind::ProductDesign),
        (&project.id, ExecutionKind::ProjectDesign),
    ] {
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(id)
                    .kind(kind)
                    .repo_remote_url("https://github.com/example/design.git")
                    .status(ExecutionStatus::WaitingHuman)
                    .build(),
            )
            .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE work_executions SET status = 'waiting_human' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    }
    let conn = db.connect().unwrap();
    crate::stuck_report::migrate_wait_history(&conn).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE property = 'execution_waiting_human'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn question_classification_uses_pre_transaction_liveness_and_rechecks_eligibility() {
    use boss_protocol::{ProposalKind, RunDoneOutcome, RunDoneProposalPayload};
    use std::cell::Cell;

    for (case, alive, expired, waiting_human, task_status, change_after_probe, expected) in [
        ("live", true, false, false, "active", false, "S15_orphan_live_pid"),
        ("dead", false, false, false, "active", false, "S2_churn_guard"),
        ("expired", true, true, false, "active", false, "S2_churn_guard"),
        ("human", true, false, true, "active", false, "S19_waiting_human"),
        ("todo", true, false, false, "todo", false, "S2_churn_guard"),
        ("replaced", true, false, false, "active", true, "S2_churn_guard"),
    ] {
        let db = WorkDb::open(temp_db_path(&format!("question-liveness-{case}"))).unwrap();
        let (_, id, execution) = make_waiting_human_chore(&db, "Question failure reason");
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE tasks SET status = ?2, dispatch_failed_reason = 'churn_guard' WHERE id = ?1",
                params![id, task_status],
            )
            .unwrap();
            if !waiting_human {
                conn.execute(
                    "UPDATE work_executions SET status = 'running' WHERE id = ?1",
                    [&execution],
                )
                .unwrap();
            }
            conn.execute(
                "UPDATE work_runs SET shell_pid = 12345, host_id = 'local', created_at = ?1 WHERE execution_id = ?2",
                params![
                    if expired {
                        now() - REDISPATCH_PID_TRUST_SECS - 1
                    } else {
                        now()
                    },
                    execution
                ],
            )
            .unwrap();
        }
        let payload = serde_json::to_string(&RunDoneProposalPayload {
            outcome: RunDoneOutcome::Blocked,
            summary: "Needs a decision".into(),
            question: Some(question()),
            queue_ci_evidence: None,
        })
        .unwrap();
        let probes = Cell::new(0);
        let outcome = db
            .submit_worker_proposal_with_liveness(
                crate::SubmitWorkerProposalInput {
                    execution_id: &execution,
                    work_item_id: &id,
                    kind: ProposalKind::RunDone,
                    payload_json: &payload,
                    idempotency_key: "question-classification",
                },
                &|pid| {
                    assert_eq!(pid, Some(12345));
                    probes.set(probes.get() + 1);
                    // Re-entering storage proves the connection lock is released.
                    let conn = db.connect().unwrap();
                    if change_after_probe {
                        conn.execute(
                            "UPDATE work_runs SET shell_pid = 54321 WHERE execution_id = ?1",
                            [&execution],
                        )
                        .unwrap();
                    }
                    alive
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(outcome.proposal.state, boss_protocol::ProposalState::Applied);
        assert_eq!(
            probes.get(),
            if expired || task_status != "active" { 0 } else { 1 },
            "{case}"
        );
        let classification: String = db
            .connect()
            .unwrap()
            .query_row(
                "SELECT stuck_classification FROM operator_questions WHERE execution_id = ?1",
                [&execution],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(classification, expected, "{case}");
        let report = db.stuck_report(604800, now(), &|_| false).unwrap();
        assert_eq!(report.questions_filed_by_stuck_kind.get(expected), Some(&1), "{case}");
    }
}
