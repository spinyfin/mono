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

#[test]
fn queued_wait_payload_names_the_actual_revision_prerequisite() {
    let (db, product_id, _) = setup_product_and_chore();
    let root = make_in_review_chore(&db, &product_id, "https://github.com/test/repo/pull/42");
    let checker = FakePrStateChecker::always(PrOpenState::Open);
    let first = db.create_revision(revision_input(&root), &checker).unwrap();
    let second = db.create_revision(revision_input(&first.id), &checker).unwrap();
    let runtime = db.get_task_runtime(&second.id).unwrap();
    assert_eq!(runtime.execution_status, Some(ExecutionStatus::WaitingDependency));
    let json = serde_json::to_value(&runtime).unwrap();
    assert_eq!(json["dispatch_wait_reason"], "waiting_dependency");
    assert_eq!(json["dispatch_wait_blocker"]["work_item_id"], first.id);
    assert_eq!(json["dispatch_wait_blocker"]["product_id"], product_id);
    assert_eq!(json["dispatch_wait_blocker"]["short_id"], first.short_id.unwrap());
    assert_eq!(json["dispatch_wait_blocker"]["kind"], "task");
    let tree = db.get_work_tree(&product_id).unwrap();
    let from_tree = tree.task_runtimes.iter().find(|r| r.work_item_id == second.id).unwrap();
    assert_eq!(serde_json::to_value(from_tree).unwrap(), json);

    db.update_work_item(
        &first.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..Default::default()
        },
    )
    .unwrap();
    let after = db.get_task_runtime(&second.id).unwrap();
    assert_eq!(after.execution_status, Some(ExecutionStatus::Ready));
    assert!(after.dispatch_wait_blocker.is_none());
    db.connect().unwrap().execute(
        "UPDATE work_executions SET status = 'claimed', dispatch_wait_reason = 'waiting_dependency', dispatch_wait_blocker_id = ?2 WHERE id = ?1",
        params![after.execution_id.unwrap(), first.id],
    ).unwrap();
    let claimed = db.get_task_runtime(&second.id).unwrap();
    assert_eq!(claimed.execution_status, Some(ExecutionStatus::Claimed));
    assert!(claimed.dispatch_wait_reason.is_none());
    assert!(claimed.dispatch_wait_blocker.is_none());
}

#[test]
fn queued_wait_payload_preserves_holds_and_clears_the_blocker() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let blocker = create_test_chore(&db, product_id, "Earlier writer");
    let blocker_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&blocker.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let execution = seed_ready_execution(&db, &chore_id);
    db.set_dispatch_wait_with_blocker(&execution.id, "chain_serialized", Some(&blocker.id))
        .unwrap();
    let wait = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(wait.dispatch_wait_reason.as_deref(), Some("chain_serialized"));
    assert_eq!(wait.dispatch_wait_blocker.unwrap().work_item_id, blocker.id);

    for reason in [
        "dispatch_paused",
        "pool_exhausted",
        "Held by the interactive concurrency cap (3/3 workers live) — dispatches as workers finish",
        "custom hold",
    ] {
        db.set_dispatch_wait_reason(&execution.id, reason).unwrap();
        let wait = db.get_task_runtime(&chore_id).unwrap();
        assert_eq!(wait.dispatch_wait_reason.as_deref(), Some(reason));
        assert!(
            wait.dispatch_wait_blocker.is_none(),
            "a new hold must clear the old link"
        );
    }
    db.set_dispatch_wait_with_blocker(&execution.id, "chain_serialized", Some(&blocker.id))
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&blocker_execution.id],
        )
        .unwrap();
    let cleared = db.get_task_runtime(&chore_id).unwrap();
    assert!(cleared.dispatch_wait_reason.is_none());
    assert!(cleared.dispatch_wait_blocker.is_none());
    db.clear_dispatch_wait_reason(&execution.id).unwrap();
    assert!(db.get_task_runtime(&chore_id).unwrap().dispatch_wait_reason.is_none());
}

#[test]
fn queued_wait_payload_exposes_backoff_without_a_prior_failure_and_hides_stale_holds() {
    let (db, _, chore_id) = setup_product_and_chore();
    let execution = seed_ready_execution(&db, &chore_id);
    let future = (boss_engine_utils::epoch_time::now_epoch_secs() + 3600).to_string();
    db.connect().unwrap().execute(
        "UPDATE work_executions SET dispatch_not_before = ?2, dispatch_wait_reason = 'pool_exhausted' WHERE id = ?1",
        params![execution.id, future],
    ).unwrap();
    let runtime = db.get_task_runtime(&chore_id).unwrap();
    assert!(runtime.dispatch_retry_at.is_none());
    let wait = runtime;
    assert_eq!(wait.dispatch_wait_reason.as_deref(), Some("not_before"));
    assert_eq!(wait.dispatch_not_before.as_deref(), Some(future.as_str()));

    for state in ["claimed", "running", "completed"] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![execution.id, state],
            )
            .unwrap();
        let json = serde_json::to_value(db.get_task_runtime(&chore_id).unwrap()).unwrap();
        assert!(
            json.get("dispatch_wait_reason").is_none()
                && json.get("dispatch_wait_blocker").is_none()
                && json.get("dispatch_not_before").is_none(),
            "stale wait in {state}"
        );
    }
}

#[test]
fn queued_wait_missing_execution_is_absent_and_missing_prerequisite_is_honest() {
    let (db, _, chore_id) = setup_product_and_chore();
    assert!(db.get_task_runtime(&chore_id).unwrap().dispatch_wait_reason.is_none());
    let execution = seed_ready_execution(&db, &chore_id);
    // Nothing gates: do not invent a dependency wait.
    db.downgrade_ready_to_waiting_dependency(&execution.id).unwrap();
    let wait = db.get_task_runtime(&chore_id).unwrap();
    assert!(wait.dispatch_wait_reason.is_none());
    assert!(wait.dispatch_wait_blocker.is_none());
    // An edge to a prerequisite row that no longer exists still gates, without a link.
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES (?1, 'task_missing', 'blocks', '1')",
            [&chore_id],
        )
        .unwrap();
    let wait = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(wait.dispatch_wait_reason.as_deref(), Some("waiting_dependency"));
    assert!(wait.dispatch_wait_blocker.is_none());
}

#[test]
fn queued_wait_chain_hold_survives_claimed_blocker_and_clears_when_terminal() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let blocker = create_test_chore(&db, product_id, "Earlier writer");
    let blocker_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&blocker.id)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let execution = seed_ready_execution(&db, &chore_id);
    db.set_dispatch_wait_with_blocker(&execution.id, "chain_serialized", Some(&blocker.id))
        .unwrap();
    for (status, held) in [
        ("claimed", true),
        ("running", true),
        ("waiting_human", true),
        ("completed", false),
    ] {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = ?2 WHERE id = ?1",
                params![blocker_execution.id, status],
            )
            .unwrap();
        let wait = db.get_task_runtime(&chore_id).unwrap();
        assert_eq!(wait.dispatch_wait_reason.is_some(), held, "reason in {status}");
        assert_eq!(wait.dispatch_wait_blocker.is_some(), held, "blocker in {status}");
    }
}

#[test]
fn queued_wait_pre_start_failure_backoff_carries_retry_and_not_before() {
    let (db, _, chore_id) = setup_product_and_chore();
    let execution = seed_ready_execution(&db, &chore_id);
    let future = (boss_engine_utils::epoch_time::now_epoch_secs() + 3600).to_string();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET dispatch_not_before = ?2, pre_start_failure_count = 1 WHERE id = ?1",
            params![execution.id, future],
        )
        .unwrap();
    let runtime = db.get_task_runtime(&chore_id).unwrap();
    assert_eq!(runtime.dispatch_retry_at.as_deref(), Some(future.as_str()));
    assert_eq!(runtime.dispatch_not_before.as_deref(), Some(future.as_str()));
}

#[test]
fn queued_wait_project_prerequisite_is_labelled_as_a_project() {
    let (db, product_id, chore_id) = setup_product_and_chore();
    let project = db
        .create_project(CreateProjectInput {
            product_id,
            name: "Prereq project".to_owned(),
            description: None,
            goal: None,
            autostart: true,
            no_design_task: true,
            design_reasoning_effort_xhigh: false,
        })
        .unwrap();
    let execution = seed_ready_execution(&db, &chore_id);
    db.downgrade_ready_to_waiting_dependency(&execution.id).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_item_dependencies (dependent_id, prerequisite_id, relation, created_at)
             VALUES (?1, ?2, 'blocks', '1')",
            params![chore_id, project.id],
        )
        .unwrap();
    let json = serde_json::to_value(db.get_task_runtime(&chore_id).unwrap()).unwrap();
    assert_eq!(json["dispatch_wait_blocker"]["work_item_id"], project.id);
    assert_eq!(json["dispatch_wait_blocker"]["kind"], "project");
}
