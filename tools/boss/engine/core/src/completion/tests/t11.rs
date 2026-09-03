//! Recovery ownership coverage for completed reviewer give-ups.

use super::*;

/// After the auto-nudge breaker trips, a completed give-up stays in Doing but
/// is reserved for review recovery rather than orphan-active implementation
/// redispatch.
#[tokio::test]
async fn pr_review_give_up_stays_doing_and_is_owned_by_review_recovery() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, pr_review_exec_id, _pr_url) = pr_review_exec_fixture(workspace.path(), None);
    let out_dir = tempdir().unwrap();
    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None))
        .handler
        .with_pr_state_checker(open_pr_checker())
        .with_structured_output_dir(out_dir.path().to_path_buf())
        .with_max_unproductive_nudges(1);

    assert!(matches!(
        handler.on_stop(&pr_review_exec_id).await,
        StopOutcome::ReviewPassAwaitingResult
    ));
    assert!(matches!(
        handler.on_stop(&pr_review_exec_id).await,
        StopOutcome::ReviewPassCompleted { .. }
    ));

    let task = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Active);
    assert!(
        db.list_attention_items(&pr_review_exec_id)
            .unwrap()
            .iter()
            .any(|item| item.kind == REVIEW_RESULT_GIVEUP_ATTENTION_KIND)
    );
    let verdict = db.review_verdict_for_execution(&pr_review_exec_id).unwrap().unwrap();
    assert_eq!(verdict.gate_outcome, crate::work::REVIEW_GATE_OUTCOME_GAVE_UP);
    assert!(!db.list_orphan_active_candidates(0).unwrap().contains(&chore_id));
    assert!(
        db.list_dead_pr_review_candidates()
            .unwrap()
            .iter()
            .any(|candidate| candidate.work_item_id == chore_id && candidate.execution_id == pr_review_exec_id)
    );
}

#[tokio::test]
async fn batch_reviewer_without_report_fails_member_without_transcript_recovery() {
    use boss_protocol::{
        ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewClassification, ReviewLanguageBucket,
        ReviewProfile,
    };

    let workspace = tempdir().unwrap();
    let pr_url = "https://github.com/spinyfin/mono/pull/88";
    let legacy_json = clean_review_result_json(pr_url);
    let (_dir, db, _product_id, chore_id, execution_id, _) =
        pr_review_exec_fixture(workspace.path(), Some(&legacy_json));
    let task_status_before = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task.status,
        other => panic!("expected task/chore, got {other:?}"),
    };
    let classification = ReviewClassification::builder()
        .changed_files(vec!["src/lib.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["src".to_owned()])
        .build();
    let (batch, members) = db
        .create_review_batch(
            crate::work::ReviewBatchCreateInput::builder()
                .cycle_root_id(chore_id.clone())
                .base_sha("base-sha")
                .classification(classification)
                .phase(ReviewBatchPhase::PreMerge)
                .pr_number(88)
                .pr_url(pr_url)
                .target_sha("head-sha")
                .build(),
            &[crate::work::ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("test-model")
                .role(ReviewBatchMemberRole::ClaudeReviewer)
                .status(ReviewBatchMemberStatus::Pending)
                .execution_id(execution_id.clone())
                .build()],
        )
        .unwrap();

    let handler = TestHarness::new(db.clone(), StubPrDetector::ok(None)).handler;
    let outcome = handler.on_stop(&execution_id).await;
    assert!(matches!(outcome, StopOutcome::ReviewPassCompleted { .. }));

    let stored = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(stored[0].id, members[0].id);
    assert_eq!(stored[0].status, ReviewBatchMemberStatus::Failed);
    assert!(stored[0].terminal_at.is_some());
    assert!(
        db.review_verdict_for_execution(&execution_id).unwrap().is_none(),
        "a batch member must not enter the legacy artifact/transcript finalizer",
    );
    let task_status_after = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(task) | WorkItem::Task(task) => task.status,
        other => panic!("expected task/chore, got {other:?}"),
    };
    assert_eq!(
        task_status_after, task_status_before,
        "batch leaf must not advance the task itself"
    );
}
