use super::*;
use crate::work::{stuck_report::latest_basis, task_status_audit};
use boss_protocol::StuckBucket;

fn now() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_secs()
}

fn row(db: &WorkDb, id: &str) -> boss_protocol::StuckRow {
    db.stuck_report(0, now())
        .unwrap()
        .rows
        .into_iter()
        .find(|r| r.work_item_id == id)
        .unwrap()
}

fn waiver(db: &WorkDb, id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT progression_grace_waived_at FROM tasks WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn missing_ledger_uses_task_age_and_does_not_prevent_daily_snapshot() {
    let db = WorkDb::open(temp_db_path("stuck-missing-ledger")).unwrap();
    let product = create_test_product_named(&db, "Legacy");
    let task = create_test_chore_manual(&db, &product.id, "Missing history");
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
    let report = db.stuck_report(0, timestamp).unwrap();
    let entry = report.rows.iter().find(|r| r.work_item_id == task.id).unwrap();
    assert_eq!(entry.last_ledger_basis, "unknown_legacy");
    assert_eq!(entry.age_in_state_secs, 100);
    assert!(entry.age_is_lower_bound);
    assert_eq!(db.stuck_daily_snapshot(timestamp).unwrap().rows.len(), 1);
}

#[test]
fn run_start_clears_dispatch_failure_without_an_explicit_start_request() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::task_status_audit_tests::run_start_clears_dispatch_failure_without_an_explicit_start_request",
    );
    let db = WorkDb::open(temp_db_path("audit-run-start-clear")).unwrap();
    let product = create_test_product_named(&db, "Run clear");
    let task = create_test_chore_manual(&db, &product.id, "Retry");
    db.bounce_dispatch_failed_to_backlog(&task.id, "lease_failed", "No lease")
        .unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.start_execution_run(&execution.id, "agent", "repo", "lease", "workspace", "/tmp/workspace")
        .unwrap();
    assert_eq!(
        latest_basis(&db.connect().unwrap(), &task.id, "dispatch_failed_reason")
            .unwrap()
            .as_deref(),
        Some("start_execution_run_on_host_with_tmux_hosting")
    );
}

#[test]
fn manual_revision_hold_preserves_caller_actor() {
    for (actor, bucket) in [
        ("human", StuckBucket::WaitingOnHuman),
        ("engine", StuckBucket::EngineStuck),
    ] {
        let db = WorkDb::open(temp_db_path("stuck-revision-actor")).unwrap();
        let product = make_revision_product(&db, actor);
        let parent = make_in_review_chore(&db, &product, "https://github.com/spinyfin/mono/pull/77");
        let revision = db
            .create_revision_as_actor(
                CreateRevisionInput::builder()
                    .parent_task_id(parent)
                    .description("Manual revision")
                    .autostart(false)
                    .build(),
                &FakePrStateChecker::always(PrOpenState::Open),
                actor,
            )
            .unwrap();
        assert_eq!(row(&db, &revision.id).bucket, bucket);
        let conn = db.connect().unwrap();
        let recorded: String = conn.query_row("SELECT actor FROM task_status_audit WHERE work_item_id = ?1 AND property = 'autostart' ORDER BY id DESC LIMIT 1", [&revision.id], |r| r.get(0)).unwrap();
        assert_eq!(recorded, actor);
    }
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
fn concurrent_daily_snapshots_share_the_winner_and_roll_over_at_utc_midnight() {
    let (_directory, path) = disk_db_path("stuck-concurrent-snapshot");
    let db = WorkDb::open(path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let day_start = now().div_euclid(86400) * 86400;
    let handles: Vec<_> = (0..2)
        .map(|offset| {
            let db = WorkDb::open(db.path.clone()).unwrap();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                db.stuck_daily_snapshot(day_start + offset).unwrap()
            })
        })
        .collect();
    let reports: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(reports[0].generated_at_epoch_s, reports[1].generated_at_epoch_s);
    assert_eq!(
        db.stuck_daily_snapshot(day_start + 86400).unwrap().generated_at_epoch_s,
        day_start + 86400
    );
    let count: i64 = db
        .connect()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM task_stuck_daily_snapshots", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

fn question() -> boss_protocol::OperatorQuestion {
    boss_protocol::OperatorQuestion {
        text: "Change the timeout?".into(),
        explanation: "The test needs a longer timeout.".into(),
        answer_type: boss_protocol::OperatorAnswerType::YesNo,
    }
}

#[test]
fn every_failure_producer_records_provenance_and_only_refusal_is_human() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::task_status_audit_tests::every_failure_producer_records_provenance_and_only_refusal_is_human",
    );
    for basis in [
        WorkerFailureBasis::DeclaredBlocked,
        WorkerFailureBasis::NudgeBreaker,
        WorkerFailureBasis::DeliveredWithoutPr,
        WorkerFailureBasis::ForeignPrBinding,
        WorkerFailureBasis::OperatorNo,
    ] {
        let db = WorkDb::open(temp_db_path(basis.as_str())).unwrap();
        let (_, id, execution) = make_waiting_human_chore(&db, basis.as_str());
        if basis == WorkerFailureBasis::OperatorNo {
            let q = question();
            crate::work::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
            db.record_worker_awaiting_operator_answer(&execution, "needs decision", &q)
                .unwrap();
            db.answer_operator_question(&id, boss_protocol::OperatorAnswer::YesNo { value: false })
                .unwrap();
        } else {
            db.record_worker_failure_with_basis(&execution, "failure", basis)
                .unwrap();
        }
        let report = row(&db, &id);
        assert_eq!(report.failure_basis.as_deref(), Some(basis.as_str()));
        assert_eq!(
            report.bucket,
            if basis == WorkerFailureBasis::OperatorNo {
                StuckBucket::WaitingOnHuman
            } else {
                StuckBucket::EngineStuck
            }
        );
        assert_eq!(
            waiver(&db, &id).is_some(),
            matches!(
                basis,
                WorkerFailureBasis::DeclaredBlocked | WorkerFailureBasis::NudgeBreaker
            )
        );
    }
}

#[test]
fn deliberate_park_waives_grace_and_counts_unconverted_even_before_age_threshold() {
    let db = WorkDb::open(temp_db_path("audit-deliberate")).unwrap();
    let product = create_test_product_named(&db, "Park");
    let task = create_test_chore_manual(&db, product.id, "Parked");
    assert!(db.bounce_deliberate_park_to_backlog(&task.id, "legacy", None));
    assert!(waiver(&db, &task.id).is_some());
    assert_eq!(row(&db, &task.id).bucket, StuckBucket::EngineStuck);
    let report = db.stuck_report(604800, now()).unwrap();
    assert!(report.rows.is_empty());
    assert_eq!(report.unconverted_declared_blockers, 1);
}

#[test]
fn repeated_failure_and_clear_then_fail_do_not_reuse_old_provenance() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::task_status_audit_tests::repeated_failure_and_clear_then_fail_do_not_reuse_old_provenance",
    );
    let db = WorkDb::open(temp_db_path("audit-repeated")).unwrap();
    let (_, id, first) = make_waiting_human_chore(&db, "Repeated");
    db.record_worker_failure_with_basis(&first, "first", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    let second = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();
    db.record_worker_failure_with_basis(&second.id, "second", WorkerFailureBasis::NudgeBreaker)
        .unwrap();
    assert_eq!(row(&db, &id).failure_basis.as_deref(), Some("nudge_breaker"));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE task_status_audit SET changed_at = ?2 WHERE work_item_id = ?1 AND property = 'blocked_reason'",
            params![id, now() - 604801],
        )
        .unwrap();
    // A later unrelated autostart event must not replace the failure producer.
    task_status_audit::execute(
        &db.connect().unwrap(),
        "unrelated",
        "human",
        "UPDATE tasks SET autostart = 1 WHERE id = ?1",
        [&id],
    )
    .unwrap();
    assert_eq!(row(&db, &id).failure_basis.as_deref(), Some("nudge_breaker"));
    assert!(row(&db, &id).age_in_state_secs >= 604801);
    assert_eq!(row(&db, &id).last_ledger_basis, "unrelated");
    let before: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        db.record_worker_failure_with_basis(&second.id, "duplicate delivery", WorkerFailureBasis::ForeignPrBinding)
            .unwrap()
            .is_none()
    );
    let after: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE work_item_id = ?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after, "terminal execution retries are idempotent");
    let next = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&id).build())
        .unwrap();
    assert_eq!(
        latest_basis(&db.connect().unwrap(), &id, "blocked_reason")
            .unwrap()
            .as_deref(),
        Some("explicit_start_failure_clear")
    );
    db.start_execution_run(&next.id, "agent-2", "repo", "lease", "workspace", "/tmp/workspace")
        .unwrap();
    db.record_worker_failure_with_basis(&next.id, "third", WorkerFailureBasis::ForeignPrBinding)
        .unwrap();
    assert_eq!(row(&db, &id).failure_basis.as_deref(), Some("foreign_pr_binding"));
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
        let tx = conn.transaction().unwrap();
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
            db.answer_operator_question(&id, boss_protocol::OperatorAnswer::YesNo { value: false })
                .unwrap();
        } else {
            db.record_worker_failure_with_basis(
                &execution,
                "Operator declined (untrusted prose)",
                WorkerFailureBasis::DeclaredBlocked,
            )
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
        drop(conn);
        assert_eq!(row(&db, &id).failure_basis.as_deref(), Some(producer));
        assert_eq!(
            row(&db, &id).bucket,
            if producer == "operator_no" {
                StuckBucket::WaitingOnHuman
            } else {
                StuckBucket::EngineStuck
            }
        );
    }
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
    let capacity = create_test_chore_manual(&db, &product, "Capacity");
    let queued = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&capacity.id).build())
        .unwrap();
    db.set_dispatch_wait_reason(&queued.id, "Waiting for an available worker (8 working)")
        .unwrap();
    assert_eq!(row(&db, &capacity.id).bucket, StuckBucket::WaitingOnCapacity);
    let dependent = create_test_chore_manual(&db, &product, "Dependent");
    db.add_dependency(AddDependencyInput {
        prerequisite: failed.clone(),
        dependent: dependent.id.clone(),
        relation: None,
    })
    .unwrap();
    assert_eq!(row(&db, &dependent.id).bucket, StuckBucket::WaitingOnDependency);
    assert_eq!(row(&db, &failed).bucket, StuckBucket::EngineStuck);
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
    let (_, live, _) = make_waiting_human_chore(&db, "Live");
    assert!(
        !db.stuck_report(0, now())
            .unwrap()
            .rows
            .iter()
            .any(|r| r.work_item_id == live)
    );
}

#[test]
fn report_age_ignores_description_edits_and_daily_snapshots_survive_repeats() {
    let db = WorkDb::open(temp_db_path("stuck-daily")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Daily");
    db.record_worker_failure_with_basis(&execution, "blocked", WorkerFailureBasis::DeclaredBlocked)
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE task_status_audit SET changed_at = ?2 WHERE work_item_id = ?1",
        params![id, now() - 604801],
    )
    .unwrap();
    drop(conn);
    db.update_task(
        &id,
        WorkItemPatch {
            description: Some("new details".into()),
            ..Default::default()
        },
        "human",
    )
    .unwrap();
    let report = db.stuck_report(604800, now()).unwrap();
    assert_eq!(report.rows.len(), 1);
    assert!(report.rows[0].age_in_state_secs >= 604801);
    assert_eq!(report.unconverted_declared_blockers, 1);
    let first = db.stuck_daily_snapshot(now()).unwrap();
    let second = db.stuck_daily_snapshot(now() + 1).unwrap();
    assert_eq!(first.generated_at_epoch_s, second.generated_at_epoch_s);
    let registry = crate::metrics::Registry::new();
    crate::stuck_metrics::register_metrics(&registry);
    crate::stuck_metrics::record(&registry, &first);
    let values = registry.gauge_snapshots();
    assert_eq!(
        values
            .iter()
            .find(|g| g.name == "task_stuck.engine_stuck")
            .unwrap()
            .value,
        1
    );
    assert!(
        values
            .iter()
            .find(|g| g.name == "task_stuck.engine_stuck_max_age_seconds")
            .unwrap()
            .value
            >= 604801
    );
}

#[test]
fn questions_keep_their_filed_classification_after_answer_and_state_changes() {
    let db = WorkDb::open(temp_db_path("audit-question-counts")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Question counts");
    let q = question();
    crate::work::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "decision", &q)
        .unwrap();
    assert_eq!(row(&db, &id).bucket, StuckBucket::WaitingOnHuman);
    db.answer_operator_question(&id, boss_protocol::OperatorAnswer::YesNo { value: false })
        .unwrap();
    let report = db.stuck_report(604800, now()).unwrap();
    assert_eq!(report.questions_filed_by_source["worker"], 1);
    assert_eq!(report.questions_filed_by_stuck_kind["S8_operator_question"], 1);
    assert_eq!(report.unconverted_declared_blockers, 0);
}

#[test]
fn legacy_backfill_does_not_reuse_an_older_execution_refusal() {
    let _audit = crate::work::task_status_audit::coverage::Fixture::new(
        "work::tests::task_status_audit_tests::legacy_backfill_does_not_reuse_an_older_execution_refusal",
    );
    let db = WorkDb::open(temp_db_path("audit-old-refusal")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Old refusal");
    let q = question();
    crate::work::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "decision", &q)
        .unwrap();
    db.answer_operator_question(&id, boss_protocol::OperatorAnswer::YesNo { value: false })
        .unwrap();
    let later = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();
    db.record_worker_failure_with_basis(
        &later.id,
        "unknown historical cause",
        WorkerFailureBasis::ForeignPrBinding,
    )
    .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("DELETE FROM task_status_audit WHERE work_item_id = ?1", [&id])
        .unwrap();
    crate::work::task_status_backfill::backfill(&conn).unwrap();
    drop(conn);
    let report = row(&db, &id);
    assert_eq!(report.failure_basis.as_deref(), Some("unknown_legacy"));
    assert_eq!(report.bucket, StuckBucket::EngineStuck);
    assert!(report.age_is_lower_bound);
}
