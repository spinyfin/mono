use super::*;
use boss_engine_storage_values::dispatch_wait::WAITING_FOR_WORKSPACE_REASON;
use boss_protocol::StuckBucket;

fn stuck_row(db: &WorkDb, id: &str) -> Option<boss_protocol::StuckRow> {
    db.stuck_report(0, boss_engine_utils::epoch_time::now_epoch_secs(), &|_| false)
        .unwrap()
        .rows
        .into_iter()
        .find(|r| r.work_item_id == id)
}

fn bucket(db: &WorkDb, id: &str) -> Option<StuckBucket> {
    stuck_row(db, id).map(|r| r.bucket)
}

fn classification(db: &WorkDb, id: &str) -> Option<String> {
    stuck_row(db, id).map(|r| r.classification)
}

fn plant_gated_execution(db: &WorkDb, work_item_id: &str, kind: ExecutionKind) {
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(work_item_id)
            .kind(kind)
            .status(ExecutionStatus::WaitingDependency)
            .build(),
    )
    .unwrap();
}

#[test]
fn related_live_executions_and_chain_waits_are_not_engine_defects() {
    for status in ["queued", "ready", "claimed", "running", "waiting_human"] {
        for relation in ["revision", "same_pr", "nested_revision"] {
            let db = WorkDb::open(temp_db_path("stuck-live-family")).unwrap();
            let product = create_test_product_named(&db, "Family");
            let parent = create_test_chore_manual(&db, &product.id, "Parent");
            let worker = create_test_chore_manual(&db, &product.id, "Fix");
            let conn = db.connect().unwrap();
            conn.execute("UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure', pr_url = 'https://example.test/pull/1' WHERE id = ?1", [&parent.id]).unwrap();
            if relation == "same_pr" {
                conn.execute(
                    "UPDATE tasks SET pr_url = 'https://example.test/pull/1' WHERE id = ?1",
                    [&worker.id],
                )
                .unwrap();
            } else {
                conn.execute("UPDATE tasks SET kind = 'revision', parent_task_id = ?1, created_via = 'ci-fix:test' WHERE id = ?2", params![parent.id, worker.id]).unwrap();
            }
            drop(conn);
            let owner = if relation == "nested_revision" {
                let nested = create_test_chore_manual(&db, &product.id, "Nested");
                db.connect()
                    .unwrap()
                    .execute(
                        "UPDATE tasks SET kind = 'revision', parent_task_id = ?1 WHERE id = ?2",
                        params![worker.id, nested.id],
                    )
                    .unwrap();
                nested.id
            } else {
                worker.id.clone()
            };
            let execution = db
                .create_execution(
                    CreateExecutionInput::builder()
                        .work_item_id(&owner)
                        .kind(ExecutionKind::RevisionImplementation)
                        .status(status.parse().unwrap())
                        .build(),
                )
                .unwrap();
            assert_eq!(bucket(&db, &parent.id), None, "{relation}: {status}");
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET blocked_reason = 'merge_conflict' WHERE id = ?1",
                    [&parent.id],
                )
                .unwrap();
            assert_eq!(bucket(&db, &parent.id), None);
            db.connect()
                .unwrap()
                .execute(
                    "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
                    [&execution.id],
                )
                .unwrap();
            assert_eq!(bucket(&db, &parent.id), Some(StuckBucket::EngineStuck));
        }
    }
    let db = WorkDb::open(temp_db_path("stuck-chain-wait")).unwrap();
    let product = create_test_product_named(&db, "Chain");
    let task = create_test_chore_manual(&db, &product.id, "Serialized");
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
        .unwrap();
    let blocker = create_test_chore_manual(&db, &product.id, "Current writer");
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(&blocker.id)
            .kind(ExecutionKind::ChoreImplementation)
            .status(ExecutionStatus::Running)
            .build(),
    )
    .unwrap();
    db.set_dispatch_wait_with_blocker(
        &execution.id,
        "blocked by Current writer (revisions on the same PR run one at a time)",
        Some(&blocker.id),
    )
    .unwrap();
    assert_eq!(bucket(&db, &task.id), None);
}

#[test]
fn waiting_human_row_is_kept_when_a_sibling_revision_is_queued() {
    for sibling_status in ["queued", "waiting_dependency"] {
        let db = WorkDb::open(temp_db_path("stuck-s19-sibling")).unwrap();
        let product = create_test_product_named(&db, "S19 sibling");
        let parent = create_test_chore_manual(&db, &product.id, "Parent");
        let waiting = create_test_chore_manual(&db, &product.id, "Waiting");
        let sibling = create_test_chore_manual(&db, &product.id, sibling_status);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET kind = 'revision', parent_task_id = ?1, created_via = 'ci-fix:test' WHERE id IN (?2, ?3)",
                params![parent.id, waiting.id, sibling.id],
            )
            .unwrap();
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&waiting.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(ExecutionStatus::WaitingHuman)
                .build(),
        )
        .unwrap();
        db.create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&sibling.id)
                .kind(ExecutionKind::RevisionImplementation)
                .status(sibling_status.parse().unwrap())
                .build(),
        )
        .unwrap();
        assert_eq!(
            bucket(&db, &waiting.id),
            Some(StuckBucket::WaitingOnHuman),
            "{sibling_status}"
        );
        assert_eq!(
            classification(&db, &waiting.id).as_deref(),
            Some("S19_waiting_human"),
            "{sibling_status}"
        );
        assert_ne!(
            bucket(&db, &sibling.id),
            Some(StuckBucket::EngineStuck),
            "{sibling_status}"
        );
    }
}

#[test]
fn workspace_capacity_and_coordinator_manual_start_are_expected_waits() {
    let db = WorkDb::open(temp_db_path("stuck-capacity-actor")).unwrap();
    let product = create_test_product_named(&db, "Capacity");
    for actor in ["human", "boss", "boothby", "progression"] {
        let task = db
            .create_chore_as_actor(
                CreateChoreInput::builder()
                    .product_id(&product.id)
                    .name(actor)
                    .autostart(false)
                    .build(),
                actor,
            )
            .unwrap();
        assert_eq!(bucket(&db, &task.id), Some(StuckBucket::WaitingOnHuman));
        db.connect()
            .unwrap()
            .execute(
                "DELETE FROM task_status_audit WHERE work_item_id = ?1 AND property = 'autostart'",
                [&task.id],
            )
            .unwrap();
        assert_eq!(bucket(&db, &task.id), Some(StuckBucket::WaitingOnHuman));
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(&task.id).build())
            .unwrap();
        db.set_dispatch_wait_reason(&execution.id, WAITING_FOR_WORKSPACE_REASON)
            .unwrap();
        assert_eq!(bucket(&db, &task.id), Some(StuckBucket::WaitingOnCapacity));
    }
}

#[test]
fn waiting_human_prerequisite_is_live() {
    let db = WorkDb::open(temp_db_path("stuck-human-prerequisite")).unwrap();
    let (product, prerequisite, _) = make_waiting_human_chore(&db, "Human prerequisite");
    let dependent = create_test_chore_manual(&db, &product, "Dependent");
    db.add_dependency(AddDependencyInput {
        prerequisite,
        dependent: dependent.id.clone(),
        relation: None,
    })
    .unwrap();
    plant_gated_execution(&db, &dependent.id, ExecutionKind::ChoreImplementation);
    assert_eq!(bucket(&db, &dependent.id), Some(StuckBucket::WaitingOnDependency));
}

#[test]
fn gated_waiting_dependency_sibling_does_not_hide_dead_revision_chain() {
    let db = WorkDb::open(temp_db_path("stuck-s13-gated-sibling")).unwrap();
    let product = create_test_product_named(&db, "S13");
    let parent = create_test_chore_manual(&db, &product.id, "Parent");
    let dead = create_test_chore_manual(&db, &product.id, "Dead revision");
    let later = create_test_chore_manual(&db, &product.id, "Later revision");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', ci_required_state = 'success',
         pr_url = 'https://example.test/pull/13' WHERE id = ?1",
            [&parent.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?1, status = 'blocked',
         blocked_reason = 'worker_failed', created_via = 'ci-fix:test' WHERE id = ?2",
            params![parent.id, dead.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET kind = 'revision', parent_task_id = ?1, status = 'blocked',
         blocked_reason = 'dependency', created_via = 'ci-fix:test' WHERE id = ?2",
            params![parent.id, later.id],
        )
        .unwrap();
    plant_gated_execution(&db, &later.id, ExecutionKind::RevisionImplementation);
    assert_eq!(bucket(&db, &dead.id), Some(StuckBucket::EngineStuck));
    assert_eq!(
        classification(&db, &dead.id).as_deref(),
        Some("S13_revision_chain:worker_failed")
    );
}

#[test]
fn approved_review_is_waiting_for_merge_but_failures_remain_defects() {
    let db = WorkDb::open(temp_db_path("stuck-review-signals")).unwrap();
    let product = create_test_product_named(&db, "Reviews");
    for (review, ci, expected) in [
        ("approved", Some("success"), StuckBucket::WaitingOnHuman),
        ("approved", Some("fail"), StuckBucket::EngineStuck),
        ("approved", Some("in_progress"), StuckBucket::EngineStuck),
        ("approved", None, StuckBucket::EngineStuck),
        ("required", Some("success"), StuckBucket::EngineStuck),
        ("changes_requested", Some("success"), StuckBucket::EngineStuck),
    ] {
        let task = create_test_chore_manual(&db, &product.id, format!("{review}-{ci:?}"));
        db.connect().unwrap().execute(
            "UPDATE tasks SET status = 'in_review', review_required_state = ?1, ci_required_state = ?2 WHERE id = ?3",
            params![review, ci, task.id],
        ).unwrap();
        assert_eq!(bucket(&db, &task.id), Some(expected.clone()), "{review} {ci:?}");
        if expected == StuckBucket::WaitingOnHuman {
            assert_eq!(classification(&db, &task.id).as_deref(), Some("awaiting_merge"));
        } else if ci != Some("fail") {
            assert_eq!(
                classification(&db, &task.id).as_deref(),
                Some("review_without_live_work")
            );
        }
    }
}

#[test]
fn behind_does_not_outrank_a_dead_review() {
    let db = WorkDb::open(temp_db_path("stuck-behind-review")).unwrap();
    let product = create_test_product_named(&db, "Behind");
    let healthy = create_test_chore_manual(&db, &product.id, "Healthy behind");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_required_state = 'approved',
         ci_required_state = 'success', pr_merge_state_status = 'BEHIND' WHERE id = ?1",
            [&healthy.id],
        )
        .unwrap();
    assert_eq!(bucket(&db, &healthy.id), Some(StuckBucket::WaitingOnHuman));
    assert_eq!(classification(&db, &healthy.id).as_deref(), Some("S18_pr_behind"));
    for (label, status, verdict) in [
        ("failed", "failed", None),
        ("unproductive", "completed", None),
        ("gave_up", "completed", Some(crate::REVIEW_GATE_OUTCOME_GAVE_UP)),
    ] {
        let task = create_test_chore_manual(&db, &product.id, label);
        db.connect()
            .unwrap()
            .execute(
                "UPDATE tasks SET status = 'in_review', review_required_state = 'required',
             ci_required_state = 'success', pr_merge_state_status = 'BEHIND' WHERE id = ?1",
                [&task.id],
            )
            .unwrap();
        let execution = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&task.id)
                    .kind(ExecutionKind::PrReview)
                    .status(status.parse().unwrap())
                    .build(),
            )
            .unwrap();
        if let Some(outcome) = verdict {
            db.connect()
                .unwrap()
                .execute(
                    "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, gate_outcome, created_at)
                 VALUES (?1, ?2, ?3, ?4, '0')",
                    params![label, execution.id, task.id, outcome],
                )
                .unwrap();
        }
        assert_eq!(bucket(&db, &task.id), Some(StuckBucket::EngineStuck), "{label}");
        assert_eq!(
            classification(&db, &task.id).as_deref(),
            Some("review_without_live_work"),
            "{label}"
        );
    }
}

#[test]
fn execution_wait_migration_preserves_ages_and_removes_ledger_coupling() {
    let db = WorkDb::open(temp_db_path("stuck-execution-history")).unwrap();
    let (_, id, execution) = make_waiting_human_chore(&db, "Wait");
    let conn = db.connect().unwrap();
    conn.execute_batch("DROP TRIGGER execution_wait_history_insert; DROP TRIGGER execution_wait_history_update; DROP TABLE execution_wait_history;").unwrap();
    crate::stuck_report::migrate_wait_history(&conn).unwrap();
    conn.execute("UPDATE task_status_audit SET changed_at = 12345, age_is_lower_bound = 0 WHERE property = 'execution_waiting_human'", []).unwrap();
    crate::execution_wait_history::migrate(&conn).unwrap();
    let timestamp: i64 = conn
        .query_row(
            "SELECT changed_at FROM execution_wait_history WHERE execution_id = ?1",
            [&execution],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(timestamp, 12345);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_status_audit WHERE property = 'execution_waiting_human'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    // Execution transitions must work without the task-property audit tables.
    conn.execute_batch("DROP TABLE task_status_audit;").unwrap();
    drop(conn);
    db.set_execution_awaiting_human(&execution, false).unwrap();
    db.set_execution_awaiting_human(&execution, true).unwrap();
    let conn = db.connect().unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM execution_wait_history WHERE execution_id = ?1",
            [&execution],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 2, "{id}");
}

#[test]
fn productive_ai_review_waits_for_human_review_but_dead_review_is_stuck() {
    let db = WorkDb::open(temp_db_path("stuck-review-lifecycle")).unwrap();
    let product = create_test_product_named(&db, "Review lifecycle");
    let task = create_test_chore_manual(&db, &product.id, "Review");
    db.connect().unwrap().execute("UPDATE tasks SET status = 'in_review', ci_required_state = 'success', review_required_state = 'required' WHERE id = ?1", [&task.id]).unwrap();
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&task.id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    assert_eq!(bucket(&db, &task.id), Some(StuckBucket::EngineStuck));
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO pr_review_verdicts (id, execution_id, work_item_id, gate_outcome, created_at)
         VALUES ('verdict', ?1, ?2, 'no_revision_needed', '0')",
            params![execution.id, task.id],
        )
        .unwrap();
    assert_eq!(bucket(&db, &task.id), Some(StuckBucket::WaitingOnHuman));
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
            [&execution.id],
        )
        .unwrap();
    assert_eq!(bucket(&db, &task.id), Some(StuckBucket::EngineStuck));
}
