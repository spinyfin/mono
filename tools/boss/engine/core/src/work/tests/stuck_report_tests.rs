use super::*;
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
    let report = db.stuck_report(0, timestamp).unwrap();
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
    let capacity = create_test_chore_manual(&db, &product, "Capacity");
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
fn minimum_age_filters_young_rows_and_negative_age_is_rejected() {
    let db = WorkDb::open(temp_db_path("stuck-min-age")).unwrap();
    let product = create_test_product_named(&db, "Ages");
    let task = create_test_chore_manual(&db, &product.id, "Fresh");
    assert!(
        db.stuck_report(0, now())
            .unwrap()
            .rows
            .iter()
            .any(|r| r.work_item_id == task.id)
    );
    assert!(
        !db.stuck_report(604800, now())
            .unwrap()
            .rows
            .iter()
            .any(|r| r.work_item_id == task.id)
    );
    assert!(db.stuck_report(-1, now()).is_err());
}

#[test]
fn questions_keep_their_filed_classification_after_answer_and_state_changes() {
    let db = WorkDb::open(temp_db_path("stuck-question-counts")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Question counts");
    let q = question();
    crate::work::pr_flow::insert_question(&db.connect().unwrap(), &execution, "proposal", &q).unwrap();
    db.record_worker_awaiting_operator_answer(&execution, "decision", &q)
        .unwrap();
    assert_eq!(row(&db, &id).bucket, StuckBucket::WaitingOnHuman);
    db.answer_operator_question(&id, boss_protocol::OperatorAnswer::YesNo { value: false })
        .unwrap();
    let report = db.stuck_report(604800, now()).unwrap();
    assert_eq!(report.questions_filed, 1);
    assert_eq!(report.questions_filed_by_stuck_kind["S8_operator_question"], 1);
    assert_eq!(report.unconverted_declared_blockers, 0);
}
