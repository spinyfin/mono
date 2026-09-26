use super::*;

#[tokio::test]
async fn review_guide_without_submission_fails_even_with_complete_codex_transcript() {
    let (dir, db) = open_db();
    let db = Arc::new(db);
    let product = create_product(&db);
    let root = create_active_chore(&db, &product, "guide producer");
    let (series, comparison) = seed_review_guide_series(&db, &root);
    let attempt = db
        .create_pr_review_guide_attempt(&series, &comparison, boss_review_guide::PROMPT_VERSION)
        .unwrap();
    let execution = db.create_pr_review_guide_execution(&comparison, "acme/widget").unwrap();
    db.bind_pr_review_guide_attempt_execution(&attempt.id, &execution.id)
        .unwrap();
    db.start_execution_run(
        &execution.id,
        "review-1",
        "mono",
        "lease-1",
        "ws-1",
        dir.path().to_str().unwrap(),
    )
    .unwrap();
    write_codex_rollout_transcript(
        &db,
        dir.path(),
        &execution.id,
        "# Guide\n## Problem\n## Implementation\n## Example\n## Review",
    );
    let harness = TestHarness::new(db.clone(), StubPrDetector::ok(None));
    harness.handler.on_stop(&execution.id).await;
    let attempt = db
        .pr_review_guide_attempt_for_execution(&execution.id)
        .unwrap()
        .unwrap();
    assert_eq!(attempt.status, "failed");
    assert!(
        attempt
            .error
            .unwrap()
            .contains("without submitting a guide via boss propose review-guide")
    );
    assert!(
        db.get_pr_review_guide_summary_for_root(&root)
            .unwrap()
            .unwrap()
            .readable_version_id
            .is_none()
    );
}
