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

/// A PR-owning row that has finished its own work and sits in review, with a
/// revision that pushed to its PR and is held `active` for review. Returns
/// `(root, revision)`.
fn root_with_held_revision(h: &Harness, name: &str, launched: bool) -> (String, String) {
    const PR: &str = "https://github.com/example/repo/pull/42";
    let (root, root_execution) = h.ready_loose_chore(&format!("PR owner {name}"));
    // One statement per connection guard: the store hands out a single
    // connection, so a guard is never held across another store call.
    let sql = |statement: &str, params: [&str; 2]| {
        h.db.connect().unwrap().execute(statement, params).unwrap();
    };
    let finish = |execution: &str| {
        sql(
            "UPDATE work_executions SET status = 'completed', finished_at = ?2 WHERE id = ?1",
            [execution, "2"],
        );
    };
    finish(&root_execution);
    sql(
        "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
        [root.as_str(), PR],
    );
    let revision = h.revision_of(&root, &format!("task_revision_{name}"));
    let implementation =
        h.db.request_execution(
            RequestExecutionInput::builder()
                .work_item_id(&revision)
                .force(launched)
                .build(),
        )
        .unwrap();
    assert_eq!(implementation.kind, ExecutionKind::RevisionImplementation);
    finish(&implementation.id);
    sql(
        "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
        [revision.as_str(), PR],
    );
    (root, revision)
}

/// Review admission for `revision`'s push to `root`'s PR, as the revision's
/// completion requests it.
fn review_of_revision_push(h: &Harness, root: &str, revision: &str, head: &str) -> ReviewBatchDispatch {
    let mut input = pre_merge_batch_input(root, head);
    input.legacy_task_id = Some(revision.to_owned());
    h.db.create_pre_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap()
}

/// A revision launched past focus pushes to a PR whose owning row is outside
/// the focus set and was never launched. The review of that push is admitted
/// and both reviewers dispatch. The same push from a revision that was not
/// launched is deferred, and a reviewer queued on its root stays held.
#[tokio::test]
async fn a_launched_revisions_review_dispatches_though_the_row_owning_its_pr_is_out_of_focus() {
    let h = harness();
    for driver in ["claude", "codex"] {
        crate::test_support::insert_host_capability(&h.db, "local", &format!("driver={driver}"), "auto");
    }
    let focused = h.project("Focused elsewhere");
    let mut focus = DispatchFocus::default();
    focus.insert(DispatchFocusTargetKind::Project, focused.id.clone());
    h.db.save_dispatch_focus(&focus).unwrap();
    h.focus_on(&[&focused]);

    let (root, revision) = root_with_held_revision(&h, "launched", true);
    let ReviewBatchDispatch::Created { batch, executions } =
        review_of_revision_push(&h, &root, &revision, "head-launched")
    else {
        panic!("the launched revision's review must be admitted through focus");
    };
    assert_eq!(batch.cycle_root_id, root);
    let reviewers: Vec<String> = executions.into_iter().map(|execution| execution.id).collect();
    assert_eq!(reviewers.len(), 2);

    let (other_root, other_revision) = root_with_held_revision(&h, "unlaunched", false);
    assert!(matches!(
        review_of_revision_push(&h, &other_root, &other_revision, "head-unlaunched"),
        ReviewBatchDispatch::AdmissionDeferred
    ));
    let held = h.ready_execution(&other_root, ExecutionKind::PrReview);

    h.drain_and_wait_for(&[&reviewers[0], &reviewers[1]]).await;

    assert_eq!(h.dispatched().await, reviewers.into_iter().collect::<HashSet<_>>());
    h.assert_held_by_focus(&held);
}
