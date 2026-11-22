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

    let outcome = handler.on_stop(&execution_id).await;
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
