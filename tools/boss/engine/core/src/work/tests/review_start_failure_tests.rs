use super::*;

/// (c) One reported + two failed-exhausted: the batch fails outright with a
/// human-visible `pr_review_quorum_failed` attention, rather than producing a
/// clean verdict from a single source.
#[test]
fn quorum_fails_the_batch_when_fewer_than_two_leaves_report() {
    let db = WorkDb::open(temp_db_path("quorum-insufficient")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");
    let executions: Vec<_> = (0..3)
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Failed)
                    .build(),
            )
            .unwrap()
        })
        .collect();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[
                member_with(
                    ReviewBatchMemberRole::ClaudeReviewer,
                    Some(executions[0].id.clone()),
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
                member_with(
                    ReviewBatchMemberRole::CodexReviewer,
                    Some(executions[1].id.clone()),
                    2,
                    ReviewBatchMemberStatus::Failed,
                ),
                member_with(
                    ReviewBatchMemberRole::GrokReviewer,
                    Some(executions[2].id.clone()),
                    2,
                    ReviewBatchMemberStatus::Failed,
                ),
            ],
        )
        .unwrap();

    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/example/repo/pull/42',
                         pr_head_sha = ?2 WHERE id = ?1",
        rusqlite::params![cycle_root.id, "head-sha"],
    )
    .unwrap();
    conn.execute("UPDATE pr_review_batches SET generation = 2 WHERE id = ?1", [&batch.id])
        .unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed', started_at = ?2 WHERE id = ?1",
        rusqlite::params![executions[0].id, "1"],
    )
    .unwrap();

    drop(conn);
    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::InsufficientQuorum));

    let updated = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(updated.status, ReviewBatchStatus::Failed);
    assert!(updated.completed_at.is_some());

    let attention =
        find_quorum_failed_attention(&db, &cycle_root.id).expect("a pr_review_quorum_failed attention must be filed");
    assert!(attention.title.to_lowercase().contains("insufficient quorum"));
    assert!(!attention.title.contains("reviewers never started"));
    assert!(attention.body_markdown.contains("head as not reviewed"));
    assert!(!attention.body_markdown.contains("AI review failed to start"));
    let card = db
        .get_work_tree(&product.id)
        .unwrap()
        .chores
        .into_iter()
        .find(|card| card.id == cycle_root.id)
        .unwrap();
    assert_eq!(card.ai_review_badge.unwrap().label, "Not reviewed: latest commit");
}

/// Reviewers that never reached a session (pre-start dispatch failures) are
/// named as such, with each role's last error, and the batch that spends the
/// last automatic generation says so; the card then shows the head as
/// "AI review failed to start" rather than merely not yet reviewed. The
/// mono#3201 shape, driven through the real refire path: every leaf of two
/// generations fails before start, is refired once by the dead-review
/// sweep, and fails before start again.
#[test]
fn quorum_failure_names_reviewers_that_never_started_and_the_spent_generation_cap() {
    let db = WorkDb::open(temp_db_path("quorum-never-started")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/example/repo/pull/42',
                    pr_head_sha = 'head-sha' WHERE id = ?1",
            rusqlite::params![cycle_root.id],
        )
        .unwrap();
    let error = "preserved work of execution exec_prior is not contained in PR head pr/42";
    let mint = |db: &WorkDb| match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly created batch, got {other:?}"),
    };
    // Permanent pre-start failure: terminal, never started, reason recorded.
    let fail_before_start = |db: &WorkDb, execution_id: &str| {
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'failed', finished_at = '2', last_error = ?2 WHERE id = ?1",
                rusqlite::params![execution_id, format!("{error} [{execution_id}]")],
            )
            .unwrap();
    };
    // Every leaf dies before start, the sweep refires each role once, and
    // the retries die the same way; the last settled role fails the batch.
    let die_before_start = |db: &WorkDb, executions: &[WorkExecution]| {
        for execution in executions {
            fail_before_start(db, &execution.id);
            assert!(matches!(
                db.retry_dead_review_batch_member(&execution.id).unwrap(),
                RetryDeadReviewBatchMember::Retried(_)
            ));
        }
        let retries: Vec<_> = db
            .list_executions(Some(&cycle_root.id))
            .unwrap()
            .into_iter()
            .filter(|execution| execution.status == ExecutionStatus::Ready)
            .collect();
        assert_eq!(retries.len(), executions.len());
        let mut outcomes = Vec::new();
        for retry in &retries {
            fail_before_start(db, &retry.id);
            outcomes.push(db.retry_dead_review_batch_member(&retry.id).unwrap());
        }
        assert!(
            matches!(outcomes.last(), Some(RetryDeadReviewBatchMember::BatchFailed)),
            "{outcomes:?}"
        );
    };

    let (first, executions) = mint(&db);
    assert_eq!(first.generation, 1);
    die_before_start(&db, &executions);
    assert_eq!(
        db.review_batch(&first.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );
    let attention = find_quorum_failed_attention(&db, &cycle_root.id).expect("attention");
    assert!(attention.title.contains("insufficient quorum"), "{}", attention.title);
    assert!(
        attention.title.contains("reviewers never started"),
        "{}",
        attention.title
    );
    for role in ["claude_reviewer", "codex_reviewer"] {
        assert!(
            attention
                .body_markdown
                .contains(&format!("`{role}` attempt 2: {error}")),
            "{}",
            attention.body_markdown
        );
    }
    assert!(
        attention.body_markdown.contains("re-admits one replacement batch"),
        "{}",
        attention.body_markdown
    );
    let card = |db: &WorkDb| {
        db.get_work_tree(&product.id)
            .unwrap()
            .chores
            .into_iter()
            .find(|c| c.id == cycle_root.id)
            .unwrap()
    };
    let first_card = card(&db);
    assert_eq!(first_card.ai_review_state.as_deref(), Some("not_reviewed"));
    assert_eq!(
        first_card.ai_review_badge.as_ref().unwrap().label,
        "Not reviewed: latest commit"
    );

    // The one automatic replacement generation dies the same way.
    let (second, executions) = mint(&db);
    assert_eq!(second.generation, 2);
    die_before_start(&db, &executions);
    let attentions: Vec<_> = db
        .list_attention_items_for_work_item(&cycle_root.id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == "pr_review_quorum_failed" && item.body_markdown.contains(&second.id))
        .collect();
    assert_eq!(attentions.len(), 1);
    let body = &attentions[0].body_markdown;
    assert!(body.contains("reviewers never started") || attentions[0].title.contains("reviewers never started"));
    assert!(body.contains("generation 2 of the 2"), "{body}");
    assert!(
        body.contains("will not schedule another review of head `head-sha`"),
        "{body}"
    );
    assert!(body.contains("\"AI review failed to start\""), "{body}");
    assert!(!body.contains("re-admits one replacement batch"), "{body}");

    let second_card = card(&db);
    assert_eq!(second_card.ai_review_state.as_deref(), Some("not_reviewed"));
    let badge = second_card.ai_review_badge.expect("badge");
    assert_eq!(badge.label, "AI review failed to start");
    assert_eq!(badge.system_image, "exclamationmark.triangle");
    assert!(badge.tooltip.contains("generation 2"), "{}", badge.tooltip);
    assert!(badge.tooltip.contains(error), "{}", badge.tooltip);

    // A new head leaves the spent batch behind: the card is simply not yet reviewed.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_head_sha = 'newer-head' WHERE id = ?1",
            rusqlite::params![cycle_root.id],
        )
        .unwrap();
    assert_eq!(card(&db).ai_review_badge.unwrap().label, "Not reviewed: latest commit");
}
