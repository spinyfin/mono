use super::*;

/// A dispatched guide with the production Codex launch tuple.
fn review_guide_execution_fixture(workspace_path: &Path) -> (TempDir, Arc<WorkDb>, String, String, String) {
    let dir = tempdir().unwrap();
    let path = dir.path().join("boss.db");
    let db = Arc::new(WorkDb::open(path).unwrap());
    let product = create_test_product(&db);
    let root = create_active_chore(&db, &product.id, "review guide finalize test");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = ?1 WHERE id = ?2",
            rusqlite::params!["https://github.com/acme/widget.git", root],
        )
        .unwrap();
    let (series_id, comparison_id) = seed_review_guide_series(&db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series_id, &comparison_id, "review-guide-v1")
        .unwrap();
    let attempt = db.dispatch_pr_review_guide_attempt(&attempt.id, &root).unwrap();
    let execution_id = attempt.execution_id.expect("dispatch binds an execution");
    let (execution, run) = db
        .start_execution_run(
            &execution_id,
            "worker-1",
            "mono",
            "lease-1",
            "mono-agent-001",
            workspace_path.to_str().unwrap(),
        )
        .unwrap();
    db.record_execution_launch_config(&execution.id, "codex", "gpt-6-astra", None)
        .unwrap();
    assert_eq!(
        crate::driver_transcript::driver_for_execution(&db, &execution.id)
            .unwrap()
            .descriptor()
            .name,
        "codex"
    );
    assert_eq!(
        crate::driver_transcript::driver_for_spawned_execution(&db, &execution.id)
            .unwrap()
            .descriptor()
            .name,
        "codex"
    );
    finish_run_worker_pane_alive(&db, &execution.id, &run.id, Some("spawned worker pane"));
    (dir, db, root, series_id, execution_id)
}

#[tokio::test]
async fn finalize_review_guide_publishes_a_valid_guide_and_advances_the_readable_pointer() {
    let workspace = tempdir().unwrap();
    let (_dir, db, root, _series_id, execution_id) = review_guide_execution_fixture(workspace.path());
    write_codex_rollout_transcript(
        &db,
        workspace.path(),
        &execution_id,
        "# Unsubmitted draft\n\n## Problem\n## Implementation\n## Example\n## Review\n",
    );
    let proposal = submit_guide(
        &db,
        &execution_id,
        "# The Guide\n\n## Problem\n## Implementation\n## Example\n## Review\n",
    );
    assert_eq!(proposal.state, boss_protocol::ProposalState::Applied);
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let execution = db.get_execution(&execution_id).unwrap();

    let outcome = handler.on_stop(&execution.id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewGuide { published: true }),
        "expected a published outcome, got {outcome:?}"
    );

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert_eq!(summary.lifecycle, "ready");
    let version_id = summary
        .readable_version_id
        .expect("a published attempt must set the readable version");
    let version = db.get_pr_review_guide_version(&version_id).unwrap().unwrap();
    assert_eq!(
        version.markdown,
        "# The Guide\n\n## Problem\n## Implementation\n## Example\n## Review"
    );
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Completed
    );
}

#[tokio::test]
async fn finalize_review_guide_fails_the_attempt_without_a_submission() {
    let workspace = tempdir().unwrap();
    let (_dir, db, root, series_id, execution_id) = review_guide_execution_fixture(workspace.path());
    // Neither a proposal nor a transcript was produced before Stop.
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let execution = db.get_execution(&execution_id).unwrap();

    let outcome = handler.on_stop(&execution.id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewGuide { published: false }),
        "expected an unpublished outcome, got {outcome:?}"
    );

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert!(
        summary.readable_version_id.is_none(),
        "a text-less run must never publish"
    );
    let live = db.live_pr_review_guide_attempts_for_series(&series_id).unwrap();
    assert!(
        live.is_empty(),
        "the attempt must have moved to a terminal (failed) status"
    );
}

#[tokio::test]
async fn finalize_review_guide_fails_the_attempt_when_the_output_does_not_validate() {
    let workspace = tempdir().unwrap();
    let (_dir, db, root, series_id, execution_id) = review_guide_execution_fixture(workspace.path());
    // No title, no section headings — `validate_guide_output` must reject
    // this as a guide even though the driver did produce assistant text.
    write_codex_rollout_transcript(&db, workspace.path(), &execution_id, "looks fine to me, shipped it");
    let proposal = submit_guide(&db, &execution_id, "looks fine to me, shipped it");
    assert_eq!(proposal.state, boss_protocol::ProposalState::Rejected);
    assert!(proposal.decision_reason.is_some());
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let execution = db.get_execution(&execution_id).unwrap();

    let outcome = handler.on_stop(&execution.id).await;
    assert!(
        matches!(outcome, StopOutcome::ReviewGuide { published: false }),
        "expected an unpublished outcome, got {outcome:?}"
    );

    let summary = db.get_pr_review_guide_summary_for_root(&root).unwrap().unwrap();
    assert!(
        summary.readable_version_id.is_none(),
        "malformed output must never become the readable guide"
    );
    let live = db.live_pr_review_guide_attempts_for_series(&series_id).unwrap();
    assert!(
        live.is_empty(),
        "the attempt must have moved to a terminal (failed) status"
    );
}

fn submit_guide(db: &WorkDb, execution_id: &str, markdown: &str) -> boss_protocol::WorkerProposal {
    let execution = db.get_execution(execution_id).unwrap();
    db.submit_worker_proposal(crate::work::SubmitWorkerProposalInput {
        execution_id,
        work_item_id: &execution.work_item_id,
        kind: boss_protocol::ProposalKind::ReviewGuide,
        payload_json: &serde_json::json!({"body_markdown": markdown}).to_string(),
        idempotency_key: "guide",
    })
    .unwrap()
    .unwrap()
    .proposal
}
