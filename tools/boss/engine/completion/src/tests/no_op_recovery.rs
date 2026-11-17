use super::*;
use boss_observation_contracts::OpenPrMergeability;

const PR: &str = "https://github.com/spinyfin/mono/pull/3639";

#[tokio::test]
async fn conflicting_parent_refuses_applied_no_op_with_body_edit_even_after_parent_push() {
    for moved in [false, true] {
        let workspace = tempdir().unwrap();
        let (_dir, db, _, _, revision_id, execution_id, _) =
            conflict_revision_fixture(workspace.path(), PR, "unchanged");
        db.set_execution_pr_body_before(&execution_id, "original description")
            .unwrap();
        db.set_execution_stop_seen(&execution_id).unwrap();
        let verifier = StubBranchVerifier::ok("boss/parent");
        verifier
            .set_head_oid(Ok(if moved { "parent-pushed" } else { "unchanged" }.into()))
            .await;
        verifier
            .set_body(Ok("description with revision verification".into()))
            .await;
        let harness = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let probes = harness.probes.clone();
        let handler = harness
            .handler
            .with_branch_verifier(verifier)
            .with_merge_probe(Arc::new(FixedMergeabilityProbe {
                mergeability: OpenPrMergeability::Conflict,
            }));
        declare_no_changes_needed(&db, &execution_id);
        assert_eq!(handler.on_stop(&execution_id).await, StopOutcome::AwaitingInput);
        assert!(db.get_execution(&execution_id).unwrap().status.is_live());
        assert_eq!(task(&db, &revision_id).status, TaskStatus::Active);
        assert!(db.ready_no_op_declaration_id(&execution_id).unwrap().is_none());
        assert_eq!(db.execution_pr_head_baseline_absorbed(&execution_id).unwrap(), moved);
        let nudges = probes.snapshot();
        assert_eq!(nudges.len(), 1);
        assert!(nudges[0].1.contains("CONFLICTING"), "{nudges:?}");
    }
}

fn task(db: &WorkDb, id: &str) -> boss_protocol::Task {
    match db.get_work_item(id).unwrap() {
        WorkItem::Task(task) | WorkItem::Chore(task) => task,
        other => panic!("expected task, got {other:?}"),
    }
}

/// An applied no-op must not disappear behind the metadata-only CI wait.
/// The next revision needs both the dependency and the PR writer slot freed.
#[tokio::test]
async fn applied_revision_no_op_with_body_edit_finishes_before_ci_and_releases_next_writer() {
    for (slug, absorbed) in ["claude", "codex", "grok"]
        .into_iter()
        .flat_map(|slug| [false, true].map(|absorbed| (slug, absorbed)))
    {
        let workspace = tempdir().unwrap();
        let (_dir, db, product_id, revision_id, execution_id) = revision_fixture(workspace.path(), PR, "unchanged");
        set_work_item_driver(&db, &revision_id, slug);
        let parent_id = task(&db, &revision_id).parent_task_id.unwrap();
        let next = db
            .create_revision(
                boss_protocol::CreateRevisionInput::builder()
                    .parent_task_id(parent_id.clone())
                    .description("Repair the failing CI check")
                    .build(),
                &crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open),
            )
            .unwrap();
        assert!(db.live_execution_elsewhere_in_chain(&next.id).unwrap().is_some());
        db.set_execution_pr_body_before(&execution_id, "original description")
            .unwrap();
        let verifier = StubBranchVerifier::ok("boss/parent");
        let head = if absorbed { "parent-pushed" } else { "unchanged" };
        verifier.set_head_oid(Ok(head.into())).await;
        if absorbed {
            db.set_execution_stop_seen(&execution_id).unwrap();
        }
        verifier
            .set_body(Ok("description with revision verification".into()))
            .await;
        let TestHarness {
            handler, cube, pane, ..
        } = TestHarness::new(db.clone(), StubPrDetector::ok(None));
        let handler = handler
            .with_branch_verifier(verifier)
            .with_merge_probe(Arc::new(FixedStateProbe(
                boss_observation_contracts::PrLifecycleState::Open(
                    boss_observation_contracts::OpenPrStatus::ci_failing(vec![]),
                ),
            )));
        declare_no_changes_needed(&db, &execution_id);
        assert_eq!(
            handler
                .finalize_declared_run_done(&execution_id, boss_protocol::RunDoneOutcome::NoChangesNeeded)
                .await,
            StopOutcome::AwaitingInput,
        );
        let outcome = handler.on_stop(&execution_id).await;
        assert!(matches!(outcome, StopOutcome::NoChangesNeeded { .. }), "{outcome:?}");
        assert_eq!(db.execution_pr_head_baseline_absorbed(&execution_id).unwrap(), absorbed);
        assert_eq!(task(&db, &revision_id).status, TaskStatus::Done);
        assert_eq!(task(&db, &parent_id).status, TaskStatus::InReview);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Completed
        );
        assert!(db.get_execution(&execution_id).unwrap().cube_lease_id.is_none());
        assert_eq!(cube.release_calls.lock().await.as_slice(), ["lease-1"]);
        assert_eq!(pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
        assert!(db.live_execution_elsewhere_in_chain(&next.id).unwrap().is_none());
        db.reconcile_product_executions(&product_id).unwrap();
        assert!(
            db.list_ready_executions()
                .unwrap()
                .iter()
                .any(|execution| execution.work_item_id == next.id)
        );
        assert_eq!(
            db.cancel_running_execution_and_demote_task(&execution_id).unwrap(),
            (false, false)
        );
        assert_eq!(task(&db, &revision_id).status, TaskStatus::Done);
    }
}

async fn recovery_handler(db: Arc<WorkDb>, workspace: &Path) -> TestHarness {
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        workspace.join("flags.toml"),
    ));
    flags.set("worker_proposals", true).unwrap();
    flags.set("run_done_proposals_seam", true).unwrap();
    let mut harness = TestHarness::new(db, StubPrDetector::ok(None));
    let verifier = StubBranchVerifier::ok("boss/parent");
    verifier.set_head_oid(Ok("unchanged".into())).await;
    harness.handler = harness.handler.with_feature_flags(flags).with_branch_verifier(verifier);
    harness
}

const FAIL_COMPLETION: &str = "CREATE TRIGGER fail_no_op BEFORE UPDATE OF status ON work_executions
    WHEN NEW.status = 'completed' BEGIN SELECT RAISE(ABORT, 'terminal write failed'); END;";

#[tokio::test]
async fn validated_no_op_survives_restart_and_finishes_without_another_worker_event() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, revision_id, execution_id) = revision_fixture(workspace.path(), PR, "unchanged");
    declare_no_changes_needed(&db, &execution_id);
    let harness = recovery_handler(db.clone(), workspace.path()).await;
    // Applied alone is insufficient, even if recovery runs while the worker is idle.
    assert_eq!(
        harness.handler.sweep_pending_finalizes(i64::MAX).await,
        PendingFinalizeSweepOutcome::default()
    );
    assert!(db.ready_no_op_declaration_id(&execution_id).unwrap().is_none());
    db.connect().unwrap().execute_batch(FAIL_COMPLETION).unwrap();
    assert_eq!(harness.handler.on_stop(&execution_id).await, StopOutcome::DbError);
    assert!(db.ready_no_op_declaration_id(&execution_id).unwrap().is_some());
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
    assert!(harness.pane.calls.lock().await.is_empty());
    drop(harness);

    // Reopen the database and recreate the handler: no retained-input cache survives.
    let path = db.path().to_owned();
    drop(db);
    let db = Arc::new(WorkDb::open(path).unwrap());
    db.connect().unwrap().execute_batch("DROP TRIGGER fail_no_op").unwrap();
    let harness = recovery_handler(db.clone(), workspace.path()).await;
    let pending_execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(harness.handler.sweep_pending_finalizes(i64::MAX).await.finalized, 1);
    assert_eq!(task(&db, &revision_id).status, TaskStatus::Done);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
    assert_eq!(harness.cube.release_calls.lock().await.as_slice(), ["lease-1"]);
    assert_eq!(harness.pane.calls.lock().await.as_slice(), [execution_id.as_str()]);
    assert_eq!(
        harness
            .handler
            .finalize_no_op_completion(&pending_execution, None, None)
            .await,
        StopOutcome::AlreadyTerminal,
    );
    assert_eq!(
        harness.handler.sweep_pending_finalizes(i64::MAX).await,
        PendingFinalizeSweepOutcome::default()
    );
}

#[tokio::test]
async fn replacement_no_op_does_not_inherit_terminal_validation() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _, _, execution_id) = revision_fixture(workspace.path(), PR, "unchanged");
    declare_no_changes_needed(&db, &execution_id);
    let harness = recovery_handler(db.clone(), workspace.path()).await;
    db.connect().unwrap().execute_batch(FAIL_COMPLETION).unwrap();
    assert_eq!(harness.handler.on_stop(&execution_id).await, StopOutcome::DbError);
    let old = db.current_no_op_declaration_id(&execution_id).unwrap().unwrap();
    db.clear_no_changes_needed_declaration(&execution_id).unwrap();
    declare_no_changes_needed(&db, &execution_id);
    assert!(!db.mark_no_op_completion_ready(&execution_id, &old).unwrap());
    assert!(db.ready_no_op_declaration_id(&execution_id).unwrap().is_none());
    db.connect().unwrap().execute_batch("DROP TRIGGER fail_no_op").unwrap();
    assert_eq!(
        harness.handler.sweep_pending_finalizes(i64::MAX).await,
        PendingFinalizeSweepOutcome::default()
    );
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
    // A recovery that already read the old validation cannot pick up a new
    // declaration between that read and its asynchronous terminal write.
    assert_eq!(
        harness
            .handler
            .finalize_no_op_completion_retaining_declaration(
                &db.get_execution(&execution_id).unwrap(),
                &old,
                None,
                None,
            )
            .await,
        StopOutcome::AwaitingInput,
    );
    assert!(db.ready_no_op_declaration_id(&execution_id).unwrap().is_none());
    drop(harness);
    let harness = recovery_handler(db.clone(), workspace.path()).await;
    assert_eq!(
        harness.handler.sweep_pending_finalizes(i64::MAX).await,
        PendingFinalizeSweepOutcome::default()
    );
    assert!(db.get_execution(&execution_id).unwrap().status.is_live());
}
