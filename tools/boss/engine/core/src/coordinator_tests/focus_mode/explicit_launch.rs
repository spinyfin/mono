use super::*;
use boss_protocol::RequestExecutionInput;

fn request_launch(h: &Harness, task: &str) {
    h.db.request_execution(RequestExecutionInput::builder().work_item_id(task).force(true).build())
        .unwrap();
}

fn reload(h: Harness) -> Harness {
    let Harness {
        _dir,
        db,
        coordinator,
        runner,
        product_id,
    } = h;
    drop(coordinator);
    drop(runner);
    drop(db);
    let db = Arc::new(WorkDb::open(_dir.path().join("boss.db")).unwrap());
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..FakeExecutionRunner::default()
    });
    let mut coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(8),
        Arc::new(FakeCubeClient::default()),
        runner.clone(),
    );
    coordinator.set_review_pool(WorkerPool::new_review(2));
    coordinator.restore_dispatch_focus(db.load_dispatch_focus().unwrap());
    Harness {
        _dir,
        db,
        coordinator: Arc::new(coordinator),
        runner,
        product_id,
    }
}

fn followup_execution(h: &Harness, root: &str, reason: &str, suffix: &str) -> String {
    match reason {
        "pr_review" => h.ready_execution(root, ExecutionKind::PrReview),
        "post_merge" => {
            let task =
                h.db.create_review_findings_followup(
                    crate::work::ReviewFindingsFollowupInsert::builder()
                        .product_id(&h.product_id)
                        .name("Post-merge findings")
                        .created_via(format!("pr_review:post_merge:{suffix}"))
                        .description("Address findings")
                        .chain_root_id(root)
                        .autostart(false)
                        .build(),
                )
                .unwrap();
            h.ready_execution(&task.id, ExecutionKind::ChoreImplementation)
        }
        _ => {
            let revision = h.revision_of(root, &format!("task_{reason}_{suffix}"));
            let source = match reason {
                "ci_fix" => "ci-fix:test",
                "merge_conflict" => "merge-conflict:test",
                _ => "pr_review:pre_merge:test",
            };
            h.db.connect()
                .unwrap()
                .execute(
                    "UPDATE tasks SET created_via = ?2 WHERE id = ?1",
                    rusqlite::params![revision, source],
                )
                .unwrap();
            h.ready_execution(&revision, ExecutionKind::RevisionImplementation)
        }
    }
}

#[tokio::test]
async fn launched_parents_followups_dispatch_after_restart_while_unlaunched_siblings_stay_held() {
    for reason in ["pr_review", "review_revision", "ci_fix", "merge_conflict", "post_merge"] {
        let h = harness();
        let focused = h.project("Focused elsewhere");
        let (launched, _) = h.ready_loose_chore("Launched past focus");
        let (sibling, _) = h.ready_loose_chore("Unlaunched sibling");
        h.focus_on(&[&focused]);
        let mut focus = DispatchFocus::default();
        focus.insert(DispatchFocusTargetKind::Project, focused.id);
        h.db.save_dispatch_focus(&focus).unwrap();
        request_launch(&h, &launched);
        h.set_pr_url(&launched, "https://github.com/spinyfin/mono/pull/501");
        h.set_pr_url(&sibling, "https://github.com/spinyfin/mono/pull/502");
        h.db.connect()
            .unwrap()
            .execute("UPDATE work_executions SET status = 'completed', finished_at = '2'", [])
            .unwrap();
        let admitted = followup_execution(&h, &launched, reason, "launched");
        let held = followup_execution(&h, &sibling, reason, "sibling");
        let h = reload(h);

        // Inherited focus admission is not an inherited pause override.
        h.coordinator.pause_dispatch_scoped(
            1_786_385_816,
            DispatchPauseOrigin::Operator,
            DispatchPauseScope::AllSpawns,
            PauseReason::new("test: operator pause").unwrap(),
        );
        h.coordinator.drain_ready_queue_for_test().await;
        assert!(h.dispatched().await.is_empty());
        assert_eq!(
            h.execution(&admitted).dispatch_wait_reason.as_deref(),
            Some("dispatch_paused")
        );
        h.coordinator.resume_dispatch();
        h.drain_and_wait_for(&[&admitted]).await;
        assert_eq!(h.dispatched().await, HashSet::from([admitted]), "{reason}");
        h.assert_held_by_focus(&held);
    }
}

#[tokio::test]
async fn inherited_focus_admission_keeps_the_interactive_concurrency_cap() {
    let h = harness();
    let focused = h.project("Focused");
    let (launched, root_execution) = h.ready_loose_chore("Explicit launch");
    request_launch(&h, &launched);
    h.set_pr_url(&launched, "https://github.com/spinyfin/mono/pull/501");
    h.db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&root_execution],
        )
        .unwrap();
    h.focus_on(&[&focused]);
    h.coordinator.set_max_concurrent_interactive_workers(1).unwrap();
    let (_, busy) = h.ready_project_task(&focused, "Occupy the slot");
    h.drain_and_wait_for(&[&busy]).await;
    let revision = followup_execution(&h, &launched, "ci_fix", "capped");
    h.coordinator.drain_ready_queue_for_test().await;
    assert_eq!(h.dispatched().await, HashSet::from([busy]));
    let capped = h.execution(&revision);
    assert_eq!(capped.status, ExecutionStatus::Ready);
    assert!(
        capped
            .dispatch_wait_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("Waiting for an available worker")),
        "{:?}",
        capped.dispatch_wait_reason
    );
}
