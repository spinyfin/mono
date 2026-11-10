use super::*;
use crate::conflict_watch::{on_conflict_detected, on_resolved};
use crate::merge_poller::PrLifecycleProbe;
use crate::work::{CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND, PrOpenState, StaticPrStateChecker};

fn probe() -> PrLifecycleProbe {
    use crate::merge_poller::{OpenPrStatus, PrLifecycleState, PrReviewState};
    PrLifecycleProbe::builder()
        .url(PR)
        .state(PrLifecycleState::Open(OpenPrStatus::conflict_only()))
        .head_ref_oid("head222")
        .base_ref_oid("base111")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build()
}

#[tokio::test]
async fn legacy_churn_stop_retains_its_scheduling_error_and_recovers() {
    use crate::work::CreateExecutionInput;
    use boss_protocol::{CreateRevisionInput, ExecutionKind, ExecutionStatus};
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, first, chore) = blocked_with_attempt(&db);
    let checker = StaticPrStateChecker(PrOpenState::Open);
    let revision = db
        .create_revision(
            CreateRevisionInput::builder()
                .parent_task_id(&chore)
                .description("Resolve merge conflict")
                .created_via(format!("merge-conflict:{}", first.id))
                .build(),
            &checker,
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&first.id, &revision.id)
        .unwrap();
    for queued in db.list_executions(Some(&revision.id)).unwrap() {
        if !queued.status.is_terminal() {
            db.cancel_execution(&queued.id).unwrap();
        }
    }
    db.create_execution(
        CreateExecutionInput::builder()
            .work_item_id(&revision.id)
            .kind(ExecutionKind::RevisionImplementation)
            .status(ExecutionStatus::Failed)
            .build(),
    )
    .unwrap();
    let error = "cube cannot move pr/42: unique content; inspect jj log before --allow-backwards";
    assert!(
        db.bounce_dispatch_failed_to_backlog(&revision.id, "cube_workspace_positioning_failed", error)
            .unwrap()
    );
    db.abandon_conflict_resolution_for_supersede(&first.id, "superseded_stale_head")
        .unwrap();
    for base in ["base-two", "base-three"] {
        let prior = db
            .insert_conflict_resolution(ConflictResolutionInsertInput {
                product_id: candidate.product_id.clone(),
                work_item_id: chore.clone(),
                pr_url: PR.into(),
                pr_number: 42,
                head_branch: "feature".into(),
                base_branch: "main".into(),
                head_sha_before: Some("head222".into()),
                base_sha_at_trigger: Some(base.into()),
            })
            .unwrap()
            .unwrap();
        db.abandon_conflict_resolution_for_supersede(&prior.id, "superseded_stale_head")
            .unwrap();
    }
    let stopped = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: candidate.product_id.clone(),
            work_item_id: chore.clone(),
            pr_url: PR.into(),
            pr_number: 42,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            head_sha_before: Some("head222".into()),
            base_sha_at_trigger: Some("base111".into()),
        })
        .unwrap()
        .unwrap();
    assert_eq!(stopped.failure_reason.as_deref(), Some(crate::work::CHURN_GUARD_REASON));
    let publisher = RecordingPublisher::default();
    let refused = ScriptCube::new(Script::GotoDivergence);
    for _ in 0..3 {
        on_conflict_detected(&db, &publisher, Some(&refused), &checker, &candidate, &probe()).await;
    }
    let stopped = db.get_conflict_resolution(&stopped.id).unwrap().unwrap();
    assert!(crate::work::conflict_workspace_failed(&stopped));
    assert_eq!(stopped.revision_task_id.as_deref(), Some(revision.id.as_str()));
    assert!(stopped.failure_reason.unwrap().contains(error));
    assert_eq!(db.list_attention_items_for_work_item(&chore).unwrap().len(), 1);
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len(),
        4
    );
    let repaired = ScriptCube::new(Script::CleanPushed);
    for _ in 0..2 {
        assert!(on_conflict_detected(&db, &publisher, Some(&repaired), &checker, &candidate, &probe()).await);
    }
    assert_eq!(
        db.get_conflict_resolution(&stopped.id).unwrap().unwrap().status,
        "succeeded"
    );
}

#[tokio::test]
async fn cube_refusal_is_one_durable_stop_until_preparation_recovers() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = WorkDb::open(path.clone()).unwrap();
    let (candidate, attempt, chore) = blocked_with_attempt(&db);
    let publisher = RecordingPublisher::default();
    let refused = ScriptCube::new(Script::GotoDivergence);
    let checker = StaticPrStateChecker(PrOpenState::Open);
    let probe = probe();
    for _ in 0..8 {
        on_conflict_detected(&db, &publisher, Some(&refused), &checker, &candidate, &probe).await;
    }
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert!(crate::work::conflict_workspace_failed(&row));
    assert!(row.revision_task_id.is_none());
    assert!(refused.rebases.lock().await.is_empty());
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len(),
        1
    );
    let items = db.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND);
    assert!(items[0].body_markdown.contains("--allow-backwards"));
    assert_eq!(items[0].status, "open");
    // Reopen the DB to prove that neither a process-local retry flag nor the
    // trace is carrying the explanation on behalf of the task/card.
    let reopened = WorkDb::open(path).unwrap();
    let WorkItem::Chore(task) = reopened.get_work_item(&chore).unwrap() else {
        panic!()
    };
    assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));
    assert!(
        task.blocked_detail
            .unwrap()
            .contains("cube will not pick a side with unique content")
    );
    assert_eq!(reopened.reconcile_orphaned_conflict_ladder_attempts().unwrap().len(), 0);

    let repaired = ScriptCube::new(Script::CleanPushed);
    assert!(on_conflict_detected(&reopened, &publisher, Some(&repaired), &checker, &candidate, &probe).await);
    assert!(
        repaired.rebases.lock().await.is_empty(),
        "recovery probe only prepares the workspace"
    );
    assert!(on_conflict_detected(&reopened, &publisher, Some(&repaired), &checker, &candidate, &probe).await);
    let row = reopened.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "succeeded");
    assert_eq!(row.resolved_by_rung, Some(1));
    let WorkItem::Chore(task) = reopened.get_work_item(&chore).unwrap() else {
        panic!()
    };
    assert!(task.blocked_detail.is_none());
    assert!(task.blocked_reason.is_none());
    let items = reopened.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].status, "resolved");
    assert!(
        items[0].body_markdown.contains("--allow-backwards"),
        "resolved history keeps the diagnostic"
    );
}

#[tokio::test]
async fn recovered_workspace_can_fall_through_to_one_worker_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, _) = blocked_with_attempt(&db);
    let publisher = RecordingPublisher::default();
    let checker = StaticPrStateChecker(PrOpenState::Open);
    db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
    let cube = ScriptCube::new(Script::RebaseErrors);
    for _ in 0..3 {
        on_conflict_detected(&db, &publisher, Some(&cube), &checker, &candidate, &probe()).await;
    }
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "pending");
    assert!(row.revision_task_id.is_some());
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&candidate.work_item_id), None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(cube.rebases.lock().await.len(), 1);
}

#[tokio::test]
async fn clean_pr_clears_workspace_failure_without_another_cube_call() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore) = blocked_with_attempt(&db);
    db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
    on_resolved(
        &db,
        &RecordingPublisher::default(),
        None,
        &candidate,
        &[],
        "MERGEABLE",
        "CLEAN",
    )
    .await;
    let WorkItem::Chore(task) = db.get_work_item(&chore).unwrap() else {
        panic!()
    };
    assert!(task.blocked_detail.is_none());
    assert_eq!(
        db.list_attention_items_for_work_item(&chore).unwrap()[0].status,
        "resolved"
    );
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "succeeded"
    );
}

#[tokio::test]
async fn workspace_recovery_is_eligible_after_head_or_base_moves() {
    for (head, base) in [("new-head", "base111"), ("head222", "new-base")] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (candidate, attempt, _) = blocked_with_attempt(&db);
        db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
        let mut moved = probe();
        moved.head_ref_oid = Some(head.into());
        moved.base_ref_oid = Some(base.into());
        let cube = ScriptCube::new(Script::CleanPushed);
        let publisher = RecordingPublisher::default();
        let checker = StaticPrStateChecker(PrOpenState::Open);
        for _ in 0..2 {
            assert!(on_conflict_detected(&db, &publisher, Some(&cube), &checker, &candidate, &moved).await);
        }
        assert_eq!(
            db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
            "succeeded"
        );
    }
}

#[tokio::test]
async fn background_workspace_refusal_holds_cooldown_without_respawning() {
    use crate::conflict_remediation::{ConflictRemediationJob, LadderRemediationJob, RemediationDisposition};
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (candidate, attempt, chore) = blocked_with_attempt(&db);
    let job = LadderRemediationJob::new(
        db.clone(),
        Arc::new(RecordingPublisher::default()),
        Arc::new(ScriptCube::new(Script::GotoDivergence)),
    )
    .with_pr_checker(Arc::new(StaticPrStateChecker(PrOpenState::Open)));
    assert_eq!(
        job.run(candidate.clone(), attempt.clone(), probe()).await,
        RemediationDisposition::RetryAfterCooldown
    );
    let failed = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert!(crate::work::conflict_workspace_failed(&failed));
    assert_eq!(
        job.run(candidate, failed, probe()).await,
        RemediationDisposition::RetryAfterCooldown
    );
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(db.list_attention_items_for_work_item(&chore).unwrap().len(), 1);
}

#[test]
fn stopped_attempt_does_not_raise_attention_after_parent_moves_on() {
    use crate::work::WorkItemPatch;
    for patch in [
        WorkItemPatch {
            status: Some("done".into()),
            ..Default::default()
        },
        WorkItemPatch {
            pr_url: Some("https://github.com/foo/bar/pull/43".into()),
            ..Default::default()
        },
    ] {
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let (_, attempt, chore) = blocked_with_attempt(&db);
        db.update_work_item(&chore, patch).unwrap();
        db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
        assert!(!db.ensure_conflict_stop_visible(&attempt.id).unwrap());
        assert!(db.list_attention_items_for_work_item(&chore).unwrap().is_empty());
    }
}

#[tokio::test]
async fn workspace_recovery_waits_for_live_writer_before_positioning() {
    use crate::work::CreateExecutionInput;
    use boss_protocol::{ExecutionKind, ExecutionStatus};
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, attempt, chore) = blocked_with_attempt(&db);
    db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
    let writer = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(&chore)
                .kind(ExecutionKind::ChoreImplementation)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let cube = ScriptCube::new(Script::CleanPushed);
    let publisher = RecordingPublisher::default();
    let checker = StaticPrStateChecker(PrOpenState::Open);
    assert!(!on_conflict_detected(&db, &publisher, Some(&cube), &checker, &candidate, &probe()).await);
    assert_eq!(*cube.lease_calls.lock().await, 0);
    assert!(cube.gotos.lock().await.is_empty());
    assert!(crate::work::conflict_workspace_failed(
        &db.get_conflict_resolution(&attempt.id).unwrap().unwrap()
    ));
    db.cancel_execution(&writer.id).unwrap();
    assert!(on_conflict_detected(&db, &publisher, Some(&cube), &checker, &candidate, &probe()).await);
    assert_eq!(*cube.lease_calls.lock().await, 1);
}

#[test]
fn churn_stop_with_old_positioning_failure_stays_terminal_when_later_attempts_died_elsewhere() {
    use boss_protocol::CreateRevisionInput;
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (candidate, first, chore) = blocked_with_attempt(&db);
    let checker = StaticPrStateChecker(PrOpenState::Open);
    let mut attempts = vec![first];
    let new_attempt = |base: &str| ConflictResolutionInsertInput {
        product_id: candidate.product_id.clone(),
        work_item_id: chore.clone(),
        pr_url: PR.into(),
        pr_number: 42,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        head_sha_before: Some("head222".into()),
        base_sha_at_trigger: Some(base.into()),
    };
    for base in ["base-two", "base-three"] {
        attempts.push(db.insert_conflict_resolution(new_attempt(base)).unwrap().unwrap());
    }
    // The oldest attempt failed positioning; the later ones died for other reasons.
    for (index, attempt) in attempts.iter().enumerate() {
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(&chore)
                    .description("Resolve merge conflict")
                    .created_via(format!("merge-conflict:{}", attempt.id))
                    .build(),
                &checker,
            )
            .unwrap();
        db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
            .unwrap();
        let reason = if index == 0 {
            "cube_workspace_positioning_failed"
        } else {
            "worker_died"
        };
        db.bounce_dispatch_failed_to_backlog(&revision.id, reason, "boom")
            .unwrap();
        db.abandon_conflict_resolution_for_supersede(&attempt.id, "superseded_stale_head")
            .unwrap();
    }
    let stopped = db.insert_conflict_resolution(new_attempt("base111")).unwrap().unwrap();
    assert_eq!(stopped.failure_reason.as_deref(), Some(crate::work::CHURN_GUARD_REASON));
    assert!(!db.retain_conflict_revision_workspace_failure(&stopped.id).unwrap());
    let row = db.get_conflict_resolution(&stopped.id).unwrap().unwrap();
    assert_eq!(row.failure_reason.as_deref(), Some(crate::work::CHURN_GUARD_REASON));
}

#[test]
fn workspace_failure_is_surfaced_when_a_foreign_blocker_owns_the_parent() {
    use crate::work::WorkItemPatch;
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (_, attempt, chore) = blocked_with_attempt(&db);
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("review_feedback".into()),
            ..Default::default()
        },
    )
    .unwrap();
    db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
    let WorkItem::Chore(task) = db.get_work_item(&chore).unwrap() else {
        panic!()
    };
    assert_eq!(task.blocked_reason.as_deref(), Some("review_feedback"));
    let items = db.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, CONFLICT_REMEDIATION_STOPPED_ATTENTION_KIND);
    assert!(items[0].body_markdown.contains("cube divergence"));
    assert!(!db.ensure_conflict_stop_visible(&attempt.id).unwrap());
}

#[test]
fn workspace_resumes_on_an_unchanged_head_are_bounded() {
    use boss_protocol::CreateRevisionInput;
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let (_, attempt, chore) = blocked_with_attempt(&db);
    let checker = StaticPrStateChecker(PrOpenState::Open);
    let mut resumed = 0;
    for _ in 0..6 {
        let revision = db
            .create_revision(
                CreateRevisionInput::builder()
                    .parent_task_id(&chore)
                    .description("Resolve merge conflict")
                    .created_via(format!("merge-conflict:{}", attempt.id))
                    .build(),
                &checker,
            )
            .unwrap();
        db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
            .unwrap();
        for queued in db.list_executions(Some(&revision.id)).unwrap() {
            if !queued.status.is_terminal() {
                db.cancel_execution(&queued.id).unwrap();
            }
        }
        db.fail_conflict_workspace(&attempt.id, "cube divergence").unwrap();
        if db
            .resume_conflict_workspace(&attempt.id, Some("head222"), Some("base111"))
            .unwrap()
            .is_some()
        {
            resumed += 1;
        }
    }
    assert_eq!(resumed, 3);
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert!(crate::work::conflict_workspace_failed(&row));
    assert!(
        db.resume_conflict_workspace(&attempt.id, Some("moved"), Some("base111"))
            .unwrap()
            .is_some(),
        "a moved head lifts the bound"
    );
}
