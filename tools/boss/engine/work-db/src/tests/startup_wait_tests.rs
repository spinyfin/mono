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
