use super::*;

#[tokio::test]
async fn revision_child_hitting_cycle_cap_records_per_head_verdict_on_chain_root_without_moving_baseline() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7370";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let head_after = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    let root_id = db.review_cycle_root_id(&revision_id);
    assert_ne!(root_id, revision_id, "revision must have a distinct chain root");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_cycle = 1, last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head_before],
        )
        .unwrap();

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head_after.into())).await;
    verifier.set_diff_line_count(Ok(40)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_max_review_cycles(1)
        .with_enable_revision_triggered_reviews(true);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "cycle cap must advance without enqueuing a reviewer; got {outcome:?}",
    );

    let (cycle, last_sha) = db.get_task_review_cycle_state(&root_id).unwrap();
    assert_eq!(cycle, 1);
    assert_eq!(last_sha.as_deref(), Some(head_before));

    let root_verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert!(
        root_verdicts
            .iter()
            .any(|v| v.gate_outcome == crate::work::REVIEW_GATE_OUTCOME_SKIPPED_CYCLE_CAP
                && v.head_sha.as_deref() == Some(head_after)),
        "expected skipped_cycle_cap verdict on chain root; got {root_verdicts:?}",
    );
    assert!(
        db.review_verdicts_for_work_item(&revision_id).unwrap().is_empty(),
        "skip verdict must be recorded on chain root, not revision child",
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head_after],
        )
        .unwrap();
    let tree = db.get_work_tree(&product_id).unwrap();
    let root_card = tree
        .tasks
        .iter()
        .chain(tree.chores.iter())
        .find(|t| t.id == root_id)
        .unwrap();
    assert_eq!(root_card.ai_review_state.as_deref(), Some("review_not_required"));
}

#[tokio::test]
async fn noop_skip_paths_record_per_head_verdict_without_moving_baseline() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7371";
    let baseline_sha = "1111111111111111111111111111111111111111";
    let empty_diff_sha = "2222222222222222222222222222222222222222";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, baseline_sha);
    let root_id = db.review_cycle_root_id(&revision_id);

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_cycle = 1, last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, baseline_sha],
        )
        .unwrap();

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(empty_diff_sha.into())).await;
    verifier.set_diff_line_count(Ok(0)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_max_review_cycles(3)
        .with_enable_revision_triggered_reviews(true);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "metadata_only_fix",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "empty-diff skip must advance without enqueuing a reviewer; got {outcome:?}",
    );

    let (cycle, last_sha) = db.get_task_review_cycle_state(&root_id).unwrap();
    assert_eq!(cycle, 1, "skip must not move review_cycle");
    assert_eq!(
        last_sha.as_deref(),
        Some(baseline_sha),
        "skip must leave the no-op baseline (last_reviewed_sha) unchanged"
    );

    let root_verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert!(
        root_verdicts
            .iter()
            .any(|v| v.gate_outcome == crate::work::REVIEW_GATE_OUTCOME_SKIPPED_NOOP
                && v.head_sha.as_deref() == Some(empty_diff_sha)),
        "expected skipped_noop verdict on chain root; got {root_verdicts:?}",
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, empty_diff_sha],
        )
        .unwrap();
    let tree = db.get_work_tree(&product_id).unwrap();
    let root_card = tree
        .tasks
        .iter()
        .chain(tree.chores.iter())
        .find(|t| t.id == root_id)
        .unwrap();
    assert_eq!(root_card.ai_review_state.as_deref(), Some("review_not_required"));
}

#[tokio::test]
async fn revision_push_with_revision_triggered_reviews_off_projects_review_not_required() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7372";
    let head_before = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let head_after = "cccccccccccccccccccccccccccccccccccccccc";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    let root_id = db.review_cycle_root_id(&revision_id);

    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&root_id])
        .unwrap();

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head_after.into())).await;
    verifier.set_diff_line_count(Ok(40)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_enable_revision_triggered_reviews(false);

    let outcome = handler.on_stop(&execution_id).await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "revision push with revision-triggered reviews off must not enqueue a reviewer; got {outcome:?}",
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head_after],
        )
        .unwrap();
    let tree = db.get_work_tree(&product_id).unwrap();
    let root_card = tree
        .tasks
        .iter()
        .chain(tree.chores.iter())
        .find(|t| t.id == root_id)
        .unwrap();
    assert_eq!(root_card.ai_review_state.as_deref(), Some("review_not_required"));
}

#[tokio::test]
async fn unmoved_findings_revision_with_revision_reviews_disabled_projects_review_not_required_and_respects_queued_review()
 {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7373";
    let head = "dddddddddddddddddddddddddddddddddddddddd";
    let (_dir, db, product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    db.mark_execution_metadata_fix_confirmed(&execution_id).unwrap();
    let root_id = db.review_cycle_root_id(&revision_id);

    let prior_review = db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(&root_id)
                .kind(crate::work::ExecutionKind::PrReview)
                .status(crate::work::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&prior_review.id],
        )
        .unwrap();
    crate::work::WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &prior_review.id,
        &root_id,
        &crate::work::ReviewVerdictInput {
            head_sha: Some(head.into()),
            findings_count: 2,
            revision_warranted: true,
            gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_verdicts SET revision_task_id = ?2 WHERE execution_id = ?1",
            rusqlite::params![prior_review.id, revision_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_head_sha = ?2, last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head],
        )
        .unwrap();

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    verifier.set_fresh_head_oid(Ok(head.into())).await;
    verifier.set_diff_line_count(Ok(10)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_enable_revision_triggered_reviews(false);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "metadata_only_fix",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "expected PrDetected, got {outcome:?}",
    );

    let caches = crate::work::board_caches::BoardCaches::default();
    let assert_board_state = |expected: &str| {
        let mut trace = boss_metrics::population_timing::PopulationTrace::new(
            product_id.clone(),
            "c1-completion",
            None,
            std::time::Instant::now(),
        );
        for tree in [
            db.get_work_tree(&product_id).unwrap(),
            db.get_work_tree_cached(&product_id, &mut trace, &caches).unwrap(),
        ] {
            let root_card = tree
                .tasks
                .iter()
                .chain(tree.chores.iter())
                .find(|t| t.id == root_id)
                .unwrap();
            assert_eq!(root_card.ai_review_state.as_deref(), Some(expected));
        }
    };
    assert_board_state("review_not_required");

    // With a queued pr_review, the same unmoved head waits as review_queued;
    // with an admission-deferred attention item (no queued execution), it
    // projects not_reviewed.
    let queued = db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(&root_id)
                .kind(crate::work::ExecutionKind::PrReview)
                .status(crate::work::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert_board_state("review_queued");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'cancelled' WHERE id = ?1",
            [&queued.id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "INSERT INTO work_attention_items(id, work_item_id, kind, status, title, body_markdown, created_at)
             VALUES ('c1-deferred', ?1, 'pr_review_admission_deferred', 'open', 't', 'b', '1')",
            [&root_id],
        )
        .unwrap();
    assert_board_state("not_reviewed");
}

#[tokio::test]
async fn unmoved_findings_revision_records_noop_skip_and_preserves_baseline() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7374";
    let head = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let (_dir, db, product_id, revision_id, execution_id) = revision_fixture(workspace.path(), parent_pr_url, head);
    db.mark_execution_metadata_fix_confirmed(&execution_id).unwrap();
    let root_id = db.review_cycle_root_id(&revision_id);

    let prior_review = db
        .create_execution(
            crate::work::CreateExecutionInput::builder()
                .work_item_id(&root_id)
                .kind(crate::work::ExecutionKind::PrReview)
                .status(crate::work::ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            [&prior_review.id],
        )
        .unwrap();
    crate::work::WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &prior_review.id,
        &root_id,
        &crate::work::ReviewVerdictInput {
            head_sha: Some(head.into()),
            findings_count: 1,
            revision_warranted: true,
            gate_outcome: crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
        },
    )
    .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_verdicts SET revision_task_id = ?2 WHERE execution_id = ?1",
            rusqlite::params![prior_review.id, revision_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_cycle = 1, pr_head_sha = ?2, last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head],
        )
        .unwrap();

    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head.into())).await;
    verifier.set_fresh_head_oid(Ok(head.into())).await;
    verifier.set_diff_line_count(Ok(10)).await;

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_branch_verifier(verifier)
        .with_pr_state_checker(open_pr_checker())
        .with_max_review_cycles(3)
        .with_enable_revision_triggered_reviews(true);

    let outcome = handler
        .finalize_pr_transition(
            &execution_id,
            parent_pr_url.to_owned(),
            WorkerPrCompletionTarget::InReview,
            "metadata_only_fix",
        )
        .await;
    assert!(
        matches!(outcome, StopOutcome::PrDetected { ref pr_url } if pr_url == parent_pr_url),
        "empty-diff skip must advance without enqueuing a reviewer; got {outcome:?}",
    );

    let (cycle, last_sha) = db.get_task_review_cycle_state(&root_id).unwrap();
    assert_eq!(cycle, 1, "skip must not move review_cycle");
    assert_eq!(
        last_sha.as_deref(),
        Some(head),
        "skip must leave last_reviewed_sha unchanged"
    );
    let verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert!(
        verdicts
            .iter()
            .any(|v| v.gate_outcome == crate::work::REVIEW_GATE_OUTCOME_SKIPPED_NOOP
                && v.head_sha.as_deref() == Some(head)),
        "expected skipped_noop verdict on chain root; got {verdicts:?}",
    );

    let caches = crate::work::board_caches::BoardCaches::default();
    let mut trace = boss_metrics::population_timing::PopulationTrace::new(
        product_id.clone(),
        "c2-completion",
        None,
        std::time::Instant::now(),
    );
    for tree in [
        db.get_work_tree(&product_id).unwrap(),
        db.get_work_tree_cached(&product_id, &mut trace, &caches).unwrap(),
    ] {
        let root_card = tree
            .tasks
            .iter()
            .chain(tree.chores.iter())
            .find(|t| t.id == root_id)
            .unwrap();
        assert_eq!(root_card.ai_review_state.as_deref(), Some("review_not_required"));
        assert!(
            root_card
                .ai_review_badge
                .as_ref()
                .unwrap()
                .tooltip
                .contains(&format!("Last reviewed {}", &head[..7]))
        );
    }
}

struct PanicsIfCalledEnqueuer;

#[async_trait]
impl ReviewBatchEnqueuer for PanicsIfCalledEnqueuer {
    async fn enqueue(
        &self,
        _work_db: &WorkDb,
        _work_item_id: &str,
        _repo_remote_url: &str,
        _pr_url: &str,
        _review_pool_size: usize,
    ) -> anyhow::Result<crate::work::ReviewBatchDispatch> {
        panic!("reviewer enqueue must not be reached when a skip gate fires");
    }
}

fn declared_delivery_flags(path: &std::path::Path) -> std::sync::Arc<crate::feature_flags::FeatureFlagsStore> {
    let flags = std::sync::Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        path.join("feature-flags.toml"),
    ));
    flags.load().unwrap();
    flags
}

#[tokio::test]
async fn declared_delivery_noop_skip_records_per_head_verdict_on_chain_root() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7375";
    let baseline_sha = "1212121212121212121212121212121212121212";
    let empty_diff_sha = "3434343434343434343434343434343434343434";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, baseline_sha);
    let root_id = db.review_cycle_root_id(&revision_id);
    assert_ne!(root_id, revision_id, "revision must have a distinct chain root");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_cycle = 1, last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, baseline_sha],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, empty_diff_sha],
        )
        .unwrap();

    let publisher = std::sync::Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(empty_diff_sha.into())).await;
    verifier.set_diff_line_count(Ok(0)).await;
    let flags_dir = tempdir().unwrap();
    let flags = declared_delivery_flags(flags_dir.path());
    let execution = db.get_execution(&execution_id).unwrap();

    crate::run_done_declaration::maybe_enqueue_declared_delivery_reviewer(
        &db,
        publisher.as_ref(),
        verifier.as_ref(),
        &PanicsIfCalledEnqueuer,
        &flags,
        3,
        0,
        1,
        &execution,
        &revision_id,
        &execution.repo_remote_url,
        parent_pr_url,
    )
    .await;

    let (cycle, last_sha) = db.get_task_review_cycle_state(&root_id).unwrap();
    assert_eq!(cycle, 1, "skip must not move review_cycle");
    assert_eq!(
        last_sha.as_deref(),
        Some(baseline_sha),
        "skip must leave last_reviewed_sha unchanged"
    );

    let root_verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert!(
        root_verdicts
            .iter()
            .any(|v| v.gate_outcome == crate::work::REVIEW_GATE_OUTCOME_SKIPPED_NOOP
                && v.head_sha.as_deref() == Some(empty_diff_sha)),
        "expected skipped_noop verdict on chain root; got {root_verdicts:?}",
    );
    assert!(
        db.review_verdicts_for_work_item(&revision_id).unwrap().is_empty(),
        "skip verdict must be recorded on chain root, not revision child",
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, empty_diff_sha],
        )
        .unwrap();
    let caches = crate::work::board_caches::BoardCaches::default();
    let mut trace = boss_metrics::population_timing::PopulationTrace::new(
        product_id.clone(),
        "declared-noop-skip",
        None,
        std::time::Instant::now(),
    );
    for tree in [
        db.get_work_tree(&product_id).unwrap(),
        db.get_work_tree_cached(&product_id, &mut trace, &caches).unwrap(),
    ] {
        let root_card = tree
            .tasks
            .iter()
            .chain(tree.chores.iter())
            .find(|t| t.id == root_id)
            .unwrap();
        assert_eq!(root_card.ai_review_state.as_deref(), Some("review_not_required"));
    }
}

#[tokio::test]
async fn declared_delivery_cycle_cap_records_per_head_verdict_on_chain_root() {
    let workspace = tempdir().unwrap();
    let parent_pr_url = "https://github.com/spinyfin/mono/pull/7376";
    let head_before = "5656565656565656565656565656565656565656";
    let head_after = "7878787878787878787878787878787878787878";
    let (_dir, db, product_id, revision_id, execution_id) =
        revision_fixture(workspace.path(), parent_pr_url, head_before);
    let root_id = db.review_cycle_root_id(&revision_id);
    assert_ne!(root_id, revision_id, "revision must have a distinct chain root");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', review_cycle = 1, last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head_before],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET pr_head_after = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, head_after],
        )
        .unwrap();

    let publisher = std::sync::Arc::new(RecordingPublisher::default());
    let verifier = StubBranchVerifier::ok("boss/exec_parent");
    verifier.set_head_oid(Ok(head_after.into())).await;
    verifier.set_diff_line_count(Ok(40)).await;
    let flags_dir = tempdir().unwrap();
    let flags = declared_delivery_flags(flags_dir.path());
    let execution = db.get_execution(&execution_id).unwrap();

    crate::run_done_declaration::maybe_enqueue_declared_delivery_reviewer(
        &db,
        publisher.as_ref(),
        verifier.as_ref(),
        &PanicsIfCalledEnqueuer,
        &flags,
        1,
        0,
        1,
        &execution,
        &revision_id,
        &execution.repo_remote_url,
        parent_pr_url,
    )
    .await;

    let (cycle, last_sha) = db.get_task_review_cycle_state(&root_id).unwrap();
    assert_eq!(cycle, 1, "cycle cap skip must not move review_cycle");
    assert_eq!(
        last_sha.as_deref(),
        Some(head_before),
        "cycle cap skip must leave last_reviewed_sha unchanged"
    );

    let root_verdicts = db.review_verdicts_for_work_item(&root_id).unwrap();
    assert!(
        root_verdicts
            .iter()
            .any(|v| v.gate_outcome == crate::work::REVIEW_GATE_OUTCOME_SKIPPED_CYCLE_CAP
                && v.head_sha.as_deref() == Some(head_after)),
        "expected skipped_cycle_cap verdict on chain root; got {root_verdicts:?}",
    );
    assert!(
        db.review_verdicts_for_work_item(&revision_id).unwrap().is_empty(),
        "skip verdict must be recorded on chain root, not revision child",
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = ?2 WHERE id = ?1",
            rusqlite::params![root_id, head_after],
        )
        .unwrap();
    let caches = crate::work::board_caches::BoardCaches::default();
    let mut trace = boss_metrics::population_timing::PopulationTrace::new(
        product_id.clone(),
        "declared-cycle-cap",
        None,
        std::time::Instant::now(),
    );
    for tree in [
        db.get_work_tree(&product_id).unwrap(),
        db.get_work_tree_cached(&product_id, &mut trace, &caches).unwrap(),
    ] {
        let root_card = tree
            .tasks
            .iter()
            .chain(tree.chores.iter())
            .find(|t| t.id == root_id)
            .unwrap();
        assert_eq!(root_card.ai_review_state.as_deref(), Some("review_not_required"));
    }
}
