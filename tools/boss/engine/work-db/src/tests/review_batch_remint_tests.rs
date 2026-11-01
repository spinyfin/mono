//! Cross-batch review admission across heads of one cycle root, and the
//! single automatic re-mint a failed pre-merge batch without a consolidated
//! verdict earns. Fixtures are local to this module.

use boss_protocol::{
    ExecutionKind, ExecutionStatus, ProposalKind, ReviewBatchMemberRole, ReviewBatchPhase, ReviewBatchStatus,
    ReviewClassification, ReviewLanguageBucket, ReviewProfile,
};

use super::*;

fn classification() -> ReviewClassification {
    ReviewClassification::builder()
        .changed_files(vec!["tools/boss/engine/pr-review/src/parsing.rs".to_owned()])
        .complexity_flags(vec![])
        .has_production_code(true)
        .metadata_missing(vec![])
        .production_languages(vec![ReviewLanguageBucket::Rust])
        .profile(ReviewProfile::Light)
        .subsystem_buckets(vec!["tools/boss/engine".to_owned()])
        .additions(12)
        .deletions(3)
        .build()
}

fn batch_input(cycle_root_id: String, target_sha: &str, phase: ReviewBatchPhase) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(phase)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha(target_sha)
        .build()
}

fn submit_review_report(
    db: &WorkDb,
    execution_id: &str,
    work_item_id: &str,
    batch_id: &str,
    target_sha: &str,
    idempotency_key: &str,
) -> SubmitWorkerProposalOutcome {
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id,
        work_item_id,
        kind: ProposalKind::ReviewReport,
        payload_json: &format!(
            r#"{{"batch_id":"{batch_id}","target_sha":"{target_sha}","report":{{"batch_id":"{batch_id}","pr_url":"https://github.com/example/repo/pull/42","target_sha":"{target_sha}","phase":"pre_merge","summary":"Clean.","coverage":{{"files_inspected":[],"files_omitted":[],"limitations":[]}},"findings":[]}}}}"#
        ),
        idempotency_key,
    })
    .unwrap()
    .unwrap()
}

/// `are_admissible_concurrent_review_batch_pair` relaxes the ordinary
/// same-work-item double-spawn guard for read-only review-batch members of
/// one cycle root — across batches, not just within one. Its false cases are
/// the safety-critical ones — a too-permissive predicate would let genuinely
/// unrelated executions run concurrently on one work item, which is
/// precisely what the guard exists to stop.
#[test]
fn admissible_concurrent_review_batch_pair_spans_batches_of_one_cycle_root_only() {
    let db = WorkDb::open(temp_db_path("review-batch-cross-batch-admission")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    let (batch_a, executions_a) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha-a", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    // Batch A reaches `supervising`, so its supervisor is the live execution
    // a later head's leaves will be compared against — the mono PR #3110
    // shape.
    for (index, execution) in executions_a.iter().enumerate() {
        submit_review_report(
            &db,
            &execution.id,
            &cycle_root.id,
            &batch_a.id,
            "head-sha-a",
            &format!("report-a-{index}"),
        );
    }
    let supervisor_a = db
        .review_batch_members(&batch_a.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .and_then(|member| member.execution_id)
        .expect("quorum must create a supervisor execution");

    // (a) A NEW head's batch under the same cycle root: its leaves must be
    // admissible against the previous batch's supervisor AND against the
    // previous batch's leaves — never "redundant".
    let (_batch_b, executions_b) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha-b", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    assert!(
        db.are_admissible_concurrent_review_batch_pair(&executions_b[0].id, &supervisor_a)
            .unwrap(),
        "a new head's leaf must run alongside the previous head's still-live supervisor"
    );
    assert!(
        db.are_admissible_concurrent_review_batch_pair(&supervisor_a, &executions_b[0].id)
            .unwrap(),
        "cross-batch admission must be symmetric"
    );
    assert!(
        db.are_admissible_concurrent_review_batch_pair(&executions_a[0].id, &executions_b[1].id)
            .unwrap(),
        "leaves of two batches on one cycle root are independent read-only reviewers"
    );

    // (b) Two supervisors of ONE batch (a retry overlapping its live
    // predecessor) remain a genuine duplicate.
    db.mark_execution_redundant(&supervisor_a).unwrap();
    let supervisor_a_retry = match db.retry_dead_review_batch_member(&supervisor_a).unwrap() {
        RetryDeadReviewBatchMember::Retried(retry) => retry.id,
        other => panic!("expected a supervisor retry, got {other:?}"),
    };
    assert!(
        !db.are_admissible_concurrent_review_batch_pair(&supervisor_a, &supervisor_a_retry)
            .unwrap(),
        "two supervisor attempts of one batch must never be admitted together"
    );

    // (c) A leaf paired with an execution that has no member row at all is
    // not a reviewer pair.
    let bare_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    assert!(
        !db.are_admissible_concurrent_review_batch_pair(&executions_a[0].id, &bare_execution.id)
            .unwrap(),
        "an execution with no batch member row must not be treated as a batch leaf"
    );

    // (d) Members of batches under DIFFERENT cycle roots are never a pair,
    // whatever work item their executions claim.
    let other_root = create_test_chore_manual(&db, product.id, "other review target");
    let executions_other = match db
        .create_pre_merge_review_batch(
            batch_input(other_root.id.clone(), "head-sha-a", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { executions, .. } => executions,
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    assert!(
        !db.are_admissible_concurrent_review_batch_pair(&executions_a[0].id, &executions_other[0].id)
            .unwrap(),
        "batches of different cycle roots must not read as concurrent reviewers of one root"
    );
}

/// A `failed` pre-merge batch that never produced a leaf report — the
/// inert-batch reaper's shape — earns exactly one automatic next generation
/// at the same target, so the head it left unreviewed gets reviewed. A
/// second such failure is terminal: the head stays "Not reviewed" with its
/// attention standing rather than looping reviewers.
#[test]
fn failed_batch_without_a_report_is_automatically_reminted_once() {
    let db = WorkDb::open(temp_db_path("review-batch-auto-remint")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let repo = "https://github.com/example/repo";

    let (batch_1, executions_1) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    assert_eq!(batch_1.generation, 1);
    fail_batch_as_reaped(&db, &batch_1.id, &executions_1);

    let (batch_2, executions_2) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("a reaped generation-1 batch must be re-minted automatically, got {other:?}"),
    };
    assert_eq!(batch_2.generation, 2);
    assert!(
        !batch_2.explicit,
        "an automatic re-mint is not an explicit review start"
    );
    assert_eq!(batch_2.target_sha, "head-sha");
    assert_eq!(executions_2.len(), 2, "the re-mint fans out fresh leaf executions");
    assert!(
        executions_2
            .iter()
            .all(|execution| execution.status == ExecutionStatus::Ready),
        "re-minted leaves must be dispatchable"
    );

    fail_batch_as_reaped(&db, &batch_2.id, &executions_2);
    match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::ExistingBatch { batch, .. } => {
            assert_eq!(batch.id, batch_2.id);
            assert_eq!(batch.status, ReviewBatchStatus::Failed);
        }
        other => panic!("generation {MAX_AUTOMATIC_PRE_MERGE_BATCH_GENERATIONS} is the automatic cap, got {other:?}"),
    }
}

/// A batch reaped after one leaf reported and the other exhausted its
/// attempts has no consolidated verdict, so the head it left unsettled keeps
/// a recovery path: the reaper fails the batch and the next admission mints
/// generation 2 at the same target.
#[test]
fn batch_reaped_after_a_partial_leaf_report_is_automatically_reminted() {
    let db = WorkDb::open(temp_db_path("review-batch-remint-after-partial-report")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let repo = "https://github.com/example/repo";

    let (batch, executions) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    submit_review_report(
        &db,
        &executions[0].id,
        &cycle_root.id,
        &batch.id,
        "head-sha",
        "report-0",
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'completed' WHERE id = ?1",
            rusqlite::params![executions[0].id],
        )
        .unwrap();
    // The other leaf exhausted its attempts and died without reporting.
    db.mark_execution_redundant(&executions[1].id).unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE pr_review_batch_members SET status = 'failed', attempt = 2
             WHERE batch_id = ?1 AND status != 'reported'",
            rusqlite::params![batch.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE pr_review_batches SET updated_at = '1' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();
    }

    assert_eq!(db.reap_inert_review_batches(0).unwrap(), vec![batch.id.clone()]);
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );

    match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch: next, .. } => assert_eq!(next.generation, 2),
        other => panic!("a reaped batch with a partial report must keep a recovery path, got {other:?}"),
    }
}

/// The automatic re-mint waits for every member execution of the failed
/// generation to settle, so a straggler still tearing down never overlaps
/// its replacement at the same target. Not an error for the automatic path:
/// the deferred-admission sweep simply retries on its next pass.
#[test]
fn automatic_remint_waits_for_the_failed_generation_to_settle() {
    let db = WorkDb::open(temp_db_path("review-batch-remint-waits")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let repo = "https://github.com/example/repo";

    let (batch, executions) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    // One leaf abandoned, the other still live when the batch is failed
    // (the reaper's dead-root branch can produce exactly this).
    db.mark_execution_redundant(&executions[0].id).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'running' WHERE id = ?1",
            rusqlite::params![executions[1].id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
            rusqlite::params![batch.id],
        )
        .unwrap();

    match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::ExistingBatch { batch: existing, .. } => assert_eq!(existing.id, batch.id),
        other => panic!("the re-mint must wait for the straggler, got {other:?}"),
    }

    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'abandoned' WHERE id = ?1",
            rusqlite::params![executions[1].id],
        )
        .unwrap();
    match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            repo,
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch: next, .. } => assert_eq!(next.generation, 2),
        other => panic!("once settled the re-mint must proceed, got {other:?}"),
    }
}

/// Put a batch in the exact state `reap_inert_review_batches`' staleness
/// branch leaves it in: every member execution terminal without reporting
/// (abandoned, as the double-spawn guard did to mono PR #3110's leaves), the
/// member rows `failed`, the batch `failed`.
fn fail_batch_as_reaped(db: &WorkDb, batch_id: &str, executions: &[WorkExecution]) {
    for execution in executions {
        db.mark_execution_redundant(&execution.id).unwrap();
    }
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE pr_review_batch_members SET status = 'failed' WHERE batch_id = ?1 AND status IN ('pending', 'running')",
        rusqlite::params![batch_id],
    )
    .unwrap();
    conn.execute(
        "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
        rusqlite::params![batch_id],
    )
    .unwrap();
}
