//! `pr_review_batches` / `pr_review_batch_members`: immutable target,
//! classification, membership, and uniqueness contracts.

use boss_protocol::{
    ProposalDecider, ProposalKind, ProposalState, ReviewBatch, ReviewBatchMemberRole, ReviewBatchMemberStatus,
    ReviewBatchPhase, ReviewBatchStatus, ReviewClassification, ReviewLanguageBucket, ReviewProfile,
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

fn member(role: ReviewBatchMemberRole, execution_id: Option<String>) -> ReviewBatchMemberCreateInput {
    ReviewBatchMemberCreateInput::builder()
        .attempt(1)
        .provider_effort("medium")
        .requested_driver(match role {
            ReviewBatchMemberRole::ClaudeReviewer => "claude",
            ReviewBatchMemberRole::CodexReviewer => "codex",
            ReviewBatchMemberRole::GrokReviewer => "grok",
            ReviewBatchMemberRole::Supervisor => "claude",
            ReviewBatchMemberRole::PostMergeReviewer => "claude",
        })
        .resolved_model("test-model")
        .role(role)
        .status(ReviewBatchMemberStatus::Pending)
        .maybe_execution_id(execution_id)
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

fn assert_member_unchanged(member: &ReviewBatchMember, status: ReviewBatchMemberStatus) {
    assert_eq!(member.status, status);
    assert_eq!(member.report_proposal_id, None);
}

/// A verdict submission is only accepted while its batch is `supervising`
/// (see `apply_review_verdict`). Tests that only exercise the proposal
/// ledger's own submission/supersession mechanics — not the full quorum flow
/// that would normally get a batch there — force the status directly rather
/// than standing up three leaf reports first.
fn force_batch_supervising(db: &WorkDb, batch_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            rusqlite::params![batch_id],
        )
        .unwrap();
}

fn verdict_payload(batch_id: &str, target_sha: &str) -> String {
    format!(
        r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"https://github.com/example/repo/pull/42","target_sha":"{target_sha}","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#
    )
}

fn post_merge_verdict_payload(batch_id: &str, merge_sha: &str) -> String {
    format!(
        r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"https://github.com/example/repo/pull/42","target_sha":"{merge_sha}","phase":"post_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#
    )
}

fn post_merge_batch_input(cycle_root_id: String, merge_sha: &str) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PostMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha(merge_sha)
        .merge_sha(merge_sha)
        .build()
}

/// Like [`member`] but with an explicit attempt/status, for driving
/// [`try_advance_review_batch_quorum_in_tx`]'s branches directly without
/// standing up the full report-submission flow.
fn member_with(
    role: ReviewBatchMemberRole,
    execution_id: Option<String>,
    attempt: i64,
    status: ReviewBatchMemberStatus,
) -> ReviewBatchMemberCreateInput {
    ReviewBatchMemberCreateInput::builder()
        .attempt(attempt)
        .provider_effort("medium")
        .requested_driver(match role {
            ReviewBatchMemberRole::ClaudeReviewer => "claude",
            ReviewBatchMemberRole::CodexReviewer => "codex",
            ReviewBatchMemberRole::GrokReviewer => "grok",
            ReviewBatchMemberRole::Supervisor => "claude",
            ReviewBatchMemberRole::PostMergeReviewer => "claude",
        })
        .resolved_model("test-model")
        .role(role)
        .status(status)
        .maybe_execution_id(execution_id)
        .build()
}

fn advance_quorum(db: &WorkDb, batch_id: &str) -> ReviewBatchQuorumOutcome {
    db.try_advance_review_batch_quorum(batch_id).unwrap()
}

fn find_quorum_failed_attention(db: &WorkDb, work_item_id: &str) -> Option<boss_protocol::WorkAttentionItem> {
    db.list_attention_items_for_work_item(work_item_id)
        .unwrap()
        .into_iter()
        .find(|item| item.kind == "pr_review_quorum_failed")
}

/// (a) All three leaves reported: a `Supervisor` member and a `Ready`
/// `pr_review` execution are created, and the batch moves to `supervising`.
#[test]
fn quorum_dispatches_supervisor_once_all_three_leaves_report() {
    let db = WorkDb::open(temp_db_path("quorum-all-reported")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let executions: Vec<_> = (0..3)
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
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
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
                member_with(
                    ReviewBatchMemberRole::GrokReviewer,
                    Some(executions[2].id.clone()),
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
            ],
        )
        .unwrap();

    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::SupervisorDispatched));

    let updated = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(updated.status, ReviewBatchStatus::Supervising);

    let members = db.review_batch_members(&batch.id).unwrap();
    let supervisor = members
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .expect("a supervisor member must be created");
    let supervisor_execution_id = supervisor
        .execution_id
        .expect("supervisor member must own an execution");
    let supervisor_execution = db.get_execution(&supervisor_execution_id).unwrap();
    assert_eq!(supervisor_execution.status, ExecutionStatus::Ready);
    assert_eq!(supervisor_execution.kind, ExecutionKind::PrReview);
}

/// (b) Two reported + one failed with its retry exhausted (`attempt >= 2`) is
/// the named two-of-three case: same dispatch as all three reporting.
#[test]
fn quorum_dispatches_supervisor_on_two_of_three_with_one_exhausted() {
    let db = WorkDb::open(temp_db_path("quorum-two-of-three")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let executions: Vec<_> = (0..3)
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
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
                    1,
                    ReviewBatchMemberStatus::Reported,
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

    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::SupervisorDispatched));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising
    );
}

/// (c) One reported + two failed-exhausted: the batch fails outright with a
/// human-visible `pr_review_quorum_failed` attention, rather than producing a
/// clean verdict from a single source.
#[test]
fn quorum_fails_the_batch_when_fewer_than_two_leaves_report() {
    let db = WorkDb::open(temp_db_path("quorum-insufficient")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let executions: Vec<_> = (0..3)
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
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

    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::InsufficientQuorum));

    let updated = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(updated.status, ReviewBatchStatus::Failed);
    assert!(updated.completed_at.is_some());

    let attention =
        find_quorum_failed_attention(&db, &cycle_root.id).expect("a pr_review_quorum_failed attention must be filed");
    assert!(attention.title.to_lowercase().contains("insufficient quorum"));
}

/// (d) One leaf still pending/running: the quorum must not act prematurely —
/// no-op, no supervisor member created.
#[test]
fn quorum_is_a_noop_while_a_leaf_is_still_in_flight() {
    let db = WorkDb::open(temp_db_path("quorum-in-flight")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let executions: Vec<_> = (0..3)
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
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
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
                member_with(
                    ReviewBatchMemberRole::GrokReviewer,
                    Some(executions[2].id.clone()),
                    1,
                    ReviewBatchMemberStatus::Running,
                ),
            ],
        )
        .unwrap();

    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::NoOp));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Collecting
    );
    assert!(
        db.review_batch_members(&batch.id)
            .unwrap()
            .iter()
            .all(|member| member.role != ReviewBatchMemberRole::Supervisor),
        "no supervisor member may be created before the quorum is decidable"
    );
}

/// (e) Calling the quorum advance twice after a dispatch must not create a
/// second supervisor member — the doc comment's claimed idempotency.
#[test]
fn quorum_dispatch_is_idempotent_across_redundant_calls() {
    let db = WorkDb::open(temp_db_path("quorum-idempotent-dispatch")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let executions: Vec<_> = (0..3)
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
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
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
                member_with(
                    ReviewBatchMemberRole::GrokReviewer,
                    Some(executions[2].id.clone()),
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
            ],
        )
        .unwrap();

    assert!(matches!(
        advance_quorum(&db, &batch.id),
        ReviewBatchQuorumOutcome::SupervisorDispatched
    ));
    // The batch is now `supervising`, so a redundant call is a plain no-op
    // per the "any other status" branch — this is what makes calling the
    // hook from multiple settle points safe.
    assert!(matches!(advance_quorum(&db, &batch.id), ReviewBatchQuorumOutcome::NoOp));

    let supervisor_count = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .filter(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .count();
    assert_eq!(supervisor_count, 1, "exactly one supervisor member must ever exist");
}

/// (f) A supervisor that settles `Failed` on attempt 1 is not yet terminal —
/// the recovery sweep still has its one retry. Quorum must no-op rather than
/// fail the batch the moment the first attempt dies.
#[test]
fn quorum_is_a_noop_when_supervisor_attempt_one_fails() {
    let db = WorkDb::open(temp_db_path("quorum-supervisor-attempt-1-failed")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member_with(
                ReviewBatchMemberRole::Supervisor,
                Some(execution.id.clone()),
                1,
                ReviewBatchMemberStatus::Failed,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);

    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::NoOp));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising
    );
    assert!(find_quorum_failed_attention(&db, &cycle_root.id).is_none());
}

/// (g) A supervisor that settles `Failed` on attempt 2 (retry exhausted)
/// fails the batch with the `pr_review_quorum_failed` attention variant.
#[test]
fn quorum_fails_the_batch_when_the_supervisor_retry_is_exhausted() {
    let db = WorkDb::open(temp_db_path("quorum-supervisor-failed")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member_with(
                ReviewBatchMemberRole::Supervisor,
                Some(execution.id.clone()),
                2,
                ReviewBatchMemberStatus::Failed,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);

    let outcome = advance_quorum(&db, &batch.id);
    assert!(matches!(outcome, ReviewBatchQuorumOutcome::InsufficientQuorum));

    let updated = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(updated.status, ReviewBatchStatus::Failed);
    assert!(updated.completed_at.is_some());

    let attention =
        find_quorum_failed_attention(&db, &cycle_root.id).expect("a pr_review_quorum_failed attention must be filed");
    assert!(attention.title.to_lowercase().contains("supervisor"));
}

/// Batch persistence preserves the raw classifier result and the resolved
/// per-member model/effort rather than referring back to mutable task policy.
#[test]
fn review_batch_round_trips_classification_and_member_policy() {
    let db = WorkDb::open(temp_db_path("review-batch-roundtrip")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    let (created, members) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[
                member(ReviewBatchMemberRole::ClaudeReviewer, Some(execution.id.clone())),
                member(ReviewBatchMemberRole::CodexReviewer, None),
                member(ReviewBatchMemberRole::GrokReviewer, None),
            ],
        )
        .unwrap();

    assert_eq!(created.classification, classification());
    assert_eq!(created.phase, ReviewBatchPhase::PreMerge);
    assert_eq!(members.len(), 3);

    let by_target = db
        .review_batch_for_target(&cycle_root.id, ReviewBatchPhase::PreMerge, "head-sha")
        .unwrap()
        .expect("batch is queryable by immutable target");
    assert_eq!(by_target.id, created.id);
    assert_eq!(by_target.classification.changed_files, classification().changed_files);

    let stored_members = db.review_batch_members(&created.id).unwrap();
    assert_eq!(stored_members.len(), 3);
    assert_eq!(stored_members[0].role, ReviewBatchMemberRole::ClaudeReviewer);
    assert_eq!(stored_members[0].execution_id.as_deref(), Some(execution.id.as_str()));
    assert_eq!(stored_members[0].provider_effort, "medium");
    assert_eq!(stored_members[0].resolved_model, "test-model");

    let by_execution = db
        .review_batch_member_for_execution(&execution.id)
        .unwrap()
        .expect("execution is linked to exactly one review member");
    assert_eq!(by_execution.id, stored_members[0].id);
}

/// These are the design's idempotency keys: one immutable target per cycle
/// phase, and one explicit retry number for each member role in a batch.
#[test]
fn review_batch_uniqueness_rules_are_enforced_by_sqlite() {
    let db = WorkDb::open(temp_db_path("review-batch-unique")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");

    let (created, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();

    let duplicate_target = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap_err();
    assert!(
        duplicate_target.to_string().to_lowercase().contains("unique"),
        "immutable target uniqueness must be SQLite-enforced: {duplicate_target}"
    );

    let duplicate_member = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "next-head-sha", ReviewBatchPhase::PreMerge),
            &[
                member(ReviewBatchMemberRole::ClaudeReviewer, None),
                member(ReviewBatchMemberRole::ClaudeReviewer, None),
            ],
        )
        .unwrap_err();
    assert!(
        duplicate_member.to_string().to_lowercase().contains("unique"),
        "member retry uniqueness must be SQLite-enforced: {duplicate_member}"
    );
    assert!(
        db.review_batch_for_target(&cycle_root.id, ReviewBatchPhase::PreMerge, "next-head-sha")
            .unwrap()
            .is_none(),
        "a failed member insert must roll the batch row back"
    );

    assert!(db.review_batch(&created.id).unwrap().is_some());
}

#[test]
fn review_batch_rejects_a_role_not_allowed_in_its_phase() {
    let db = WorkDb::open(temp_db_path("review-batch-role-phase")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root.id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PostMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha("merge-sha")
        .merge_sha("merge-sha")
        .build();

    let error = db
        .create_review_batch(input, &[member(ReviewBatchMemberRole::ClaudeReviewer, None)])
        .unwrap_err();
    assert!(error.to_string().contains("invalid for post_merge batch"));
}

#[test]
fn review_batch_rejects_invalid_phase_merge_sha_combinations_and_empty_members() {
    let db = WorkDb::open(temp_db_path("review-batch-invariants")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");

    let missing_merge_sha = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "merge-sha", ReviewBatchPhase::PostMerge),
            &[member(ReviewBatchMemberRole::PostMergeReviewer, None)],
        )
        .unwrap_err();
    assert!(missing_merge_sha.to_string().contains("require a merge SHA"));

    let pre_merge_with_merge_sha = ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root.id.clone())
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha("head-sha")
        .merge_sha("merge-sha")
        .build();
    let unexpected_merge_sha = db
        .create_review_batch(
            pre_merge_with_merge_sha,
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap_err();
    assert!(
        unexpected_merge_sha
            .to_string()
            .contains("must not include a merge SHA")
    );

    let empty_members = db
        .create_review_batch(batch_input(cycle_root.id, "head-sha", ReviewBatchPhase::PreMerge), &[])
        .unwrap_err();
    assert!(empty_members.to_string().contains("require at least one member"));
}

#[test]
fn review_report_is_accepted_only_into_its_own_batch_member() {
    let db = WorkDb::open(temp_db_path("review-report-proposal")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, members) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::CodexReviewer, Some(execution.id.clone()))],
        )
        .unwrap();
    let member = &members[0];

    let outcome = submit_review_report(&db, &execution.id, &cycle_root.id, &batch.id, "head-sha", "report-1");

    assert_eq!(outcome.proposal.state, ProposalState::Applied);
    assert_eq!(outcome.proposal.decided_by, Some(ProposalDecider::Policy));
    assert_eq!(outcome.proposal.applied_ref.as_deref(), Some(member.id.as_str()));
    let stored = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(stored[0].status, ReviewBatchMemberStatus::Reported);
    assert_eq!(
        stored[0].report_proposal_id.as_deref(),
        Some(outcome.proposal.id.as_str())
    );
    assert!(stored[0].terminal_at.is_some());
}

#[test]
fn review_report_rejections_preserve_batch_members() {
    let db = WorkDb::open(temp_db_path("review-report-rejections")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let owner = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let other = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let failed_owner = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, members) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::CodexReviewer, Some(owner.id.clone()))],
        )
        .unwrap();
    let owned_member = &members[0];

    let (other_batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "other-head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::GrokReviewer, Some(other.id.clone()))],
        )
        .unwrap();
    let wrong_batch = submit_review_report(
        &db,
        &owner.id,
        &cycle_root.id,
        &other_batch.id,
        "other-head-sha",
        "wrong-batch",
    );
    assert_eq!(wrong_batch.proposal.state, ProposalState::Rejected);
    assert_eq!(
        wrong_batch.proposal.decision_reason.as_deref(),
        Some(format!("this execution is not a member of review batch `{}`", other_batch.id).as_str())
    );
    assert_member_unchanged(
        &db.review_batch_members(&other_batch.id).unwrap()[0],
        ReviewBatchMemberStatus::Pending,
    );

    let wrong_owner = submit_review_report(&db, &other.id, &cycle_root.id, &batch.id, "head-sha", "wrong-owner");
    assert_eq!(wrong_owner.proposal.state, ProposalState::Rejected);
    assert_eq!(
        wrong_owner.proposal.decision_reason.as_deref(),
        Some(format!("this execution is not a member of review batch `{}`", batch.id).as_str())
    );
    assert_member_unchanged(
        &db.review_batch_members(&batch.id).unwrap()[0],
        ReviewBatchMemberStatus::Pending,
    );

    let accepted = submit_review_report(&db, &owner.id, &cycle_root.id, &batch.id, "head-sha", "accepted");
    let repeated = submit_review_report(
        &db,
        &owner.id,
        &cycle_root.id,
        &batch.id,
        "head-sha",
        "second-submission",
    );
    assert_eq!(repeated.proposal.state, ProposalState::Rejected);
    assert_eq!(
        repeated.proposal.decision_reason.as_deref(),
        Some(
            format!(
                "review batch member `{}` already accepted report proposal `{}`",
                owned_member.id, accepted.proposal.id
            )
            .as_str()
        )
    );
    let reported = &db.review_batch_members(&batch.id).unwrap()[0];
    assert_eq!(reported.status, ReviewBatchMemberStatus::Reported);
    assert_eq!(
        reported.report_proposal_id.as_deref(),
        Some(accepted.proposal.id.as_str())
    );

    let (failed_batch, failed_members) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "failed-head-sha", ReviewBatchPhase::PreMerge),
            &[member(
                ReviewBatchMemberRole::ClaudeReviewer,
                Some(failed_owner.id.clone()),
            )],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batch_members SET status = 'failed' WHERE id = ?1",
            [&failed_members[0].id],
        )
        .unwrap();
    let failed = submit_review_report(
        &db,
        &failed_owner.id,
        &cycle_root.id,
        &failed_batch.id,
        "failed-head-sha",
        "failed-member",
    );
    assert_eq!(failed.proposal.state, ProposalState::Rejected);
    assert_eq!(
        failed.proposal.decision_reason.as_deref(),
        Some(
            format!(
                "review batch member `{}` cannot accept a report while status is `failed`",
                failed_members[0].id
            )
            .as_str()
        )
    );
    assert_member_unchanged(
        &db.review_batch_members(&failed_batch.id).unwrap()[0],
        ReviewBatchMemberStatus::Failed,
    );
}

#[test]
fn missing_review_report_marks_only_its_member_failed() {
    let db = WorkDb::open(temp_db_path("missing-review-report")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, members) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[
                member(ReviewBatchMemberRole::ClaudeReviewer, Some(execution.id.clone())),
                member(ReviewBatchMemberRole::CodexReviewer, None),
            ],
        )
        .unwrap();

    assert!(db.fail_review_batch_member_for_execution(&execution.id).unwrap());
    assert!(!db.fail_review_batch_member_for_execution(&execution.id).unwrap());

    let stored = db.review_batch_members(&batch.id).unwrap();
    let failed = stored.iter().find(|member| member.id == members[0].id).unwrap();
    let pending = stored.iter().find(|member| member.id == members[1].id).unwrap();
    assert_eq!(failed.status, ReviewBatchMemberStatus::Failed);
    assert!(failed.terminal_at.is_some());
    assert_eq!(pending.status, ReviewBatchMemberStatus::Pending);
    assert!(pending.terminal_at.is_none());
}

#[test]
fn review_verdict_stages_proposed_and_moves_the_batch_to_applying() {
    let db = WorkDb::open(temp_db_path("review-verdict-proposal")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::Supervisor, Some(execution.id.clone()))],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);

    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    assert_eq!(outcome.proposal.state, ProposalState::Proposed);
    assert_eq!(outcome.proposal.applied_ref, None);
    let applying = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(applying.status, ReviewBatchStatus::Applying);
    assert_eq!(
        applying.final_verdict_proposal_id.as_deref(),
        Some(outcome.proposal.id.as_str())
    );
    assert!(applying.completed_at.is_none());

    let member = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .unwrap();
    assert_eq!(member.status, ReviewBatchMemberStatus::Reported);
    assert_eq!(member.report_proposal_id.as_deref(), Some(outcome.proposal.id.as_str()));
}

/// A second, corrected verdict submission from the SAME supervisor member
/// that already staged this exact batch is accepted, not rejected: it
/// supersedes the first (still-undecided) verdict and re-points the batch
/// at the corrected one. Once the reconciler has applied the verdict the
/// batch is `completed` and this path is closed.
#[test]
fn a_corrected_review_verdict_from_the_same_supervisor_supersedes_the_prior_one() {
    let db = WorkDb::open(temp_db_path("review-verdict-correction")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::Supervisor, Some(execution.id.clone()))],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);

    let first = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.proposal.state, ProposalState::Proposed);

    let second = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-2",
        })
        .unwrap()
        .unwrap();
    assert_eq!(second.proposal.state, ProposalState::Proposed);
    assert_eq!(second.proposal.applied_ref, None);

    let applying = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(applying.status, ReviewBatchStatus::Applying);
    assert_eq!(
        applying.final_verdict_proposal_id.as_deref(),
        Some(second.proposal.id.as_str()),
        "the batch must now point at the corrected verdict"
    );

    let member = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .unwrap();
    assert_eq!(member.report_proposal_id.as_deref(), Some(second.proposal.id.as_str()));

    let prior = db
        .list_worker_proposals_for_execution(&execution.id, ProposalKind::ReviewVerdict)
        .unwrap()
        .into_iter()
        .find(|proposal| proposal.id == first.proposal.id)
        .expect("the prior verdict proposal must still exist");
    assert_eq!(prior.state, ProposalState::Superseded);
}

/// A resubmission from a DIFFERENT member/execution than the one that
/// completed the batch must still be rejected outright — the correction
/// path is keyed on the exact member and exact prior proposal, not merely
/// on the batch being `completed`.
#[test]
fn a_review_verdict_from_a_different_execution_is_rejected_even_after_completion() {
    let db = WorkDb::open(temp_db_path("review-verdict-foreign-after-completion")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let other_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::Supervisor, Some(execution.id.clone()))],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);

    let first = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.proposal.state, ProposalState::Proposed);

    // `other_execution` is not a member of this batch at all, so it hits the
    // "not a member" rejection rather than the batch-status one — either
    // way, it must never be accepted as a correction.
    let foreign = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &other_execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-foreign",
        })
        .unwrap()
        .unwrap();
    assert_eq!(foreign.proposal.state, ProposalState::Rejected);
    assert!(
        foreign
            .proposal
            .decision_reason
            .as_deref()
            .unwrap_or_default()
            .contains("not a member")
    );
    assert_eq!(
        db.review_batch(&batch.id)
            .unwrap()
            .unwrap()
            .final_verdict_proposal_id
            .as_deref(),
        Some(first.proposal.id.as_str()),
        "the foreign rejection must not disturb the already-staged batch"
    );
}

/// A verdict submitted before the batch has actually reached `supervising`
/// (e.g. still `collecting` — the quorum hasn't dispatched a supervisor yet)
/// must be rejected, not silently accepted for a batch that isn't ready for
/// one.
#[test]
fn review_verdict_is_rejected_while_batch_is_not_supervising() {
    let db = WorkDb::open(temp_db_path("review-verdict-wrong-status")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::Supervisor, Some(execution.id.clone()))],
        )
        .unwrap();
    // Deliberately NOT calling `force_batch_supervising` — the batch is
    // still `collecting`, matching `create_review_batch`'s default status.

    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    assert_eq!(outcome.proposal.state, ProposalState::Rejected);
    assert!(
        outcome
            .proposal
            .decision_reason
            .as_deref()
            .unwrap_or_default()
            .contains("not `supervising`")
    );
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().final_verdict_proposal_id,
        None
    );
}

#[test]
fn leaf_dispatch_creates_three_atomic_role_pinned_executions() {
    let db = WorkDb::open(temp_db_path("review-batch-leaf-dispatch")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge);

    let (batch, executions) = match db
        .create_pre_merge_review_batch(input.clone(), "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    assert_eq!(executions.len(), 3);
    assert!(
        executions
            .iter()
            .all(|execution| execution.status == ExecutionStatus::Ready)
    );

    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(members.len(), 3);
    assert_eq!(
        members
            .iter()
            .map(|member| (
                member.role,
                member.requested_driver.as_str(),
                member.provider_effort.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            (ReviewBatchMemberRole::ClaudeReviewer, "claude", "medium"),
            (ReviewBatchMemberRole::CodexReviewer, "codex", "medium"),
            (ReviewBatchMemberRole::GrokReviewer, "grok", "medium"),
        ]
    );
    assert!(
        db.are_same_review_batch_leaves(&executions[0].id, &executions[1].id)
            .unwrap()
    );

    match db
        .create_pre_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::ExistingBatch {
            batch: existing,
            executions: existing_executions,
        } => {
            assert_eq!(existing.id, batch.id);
            assert_eq!(existing_executions.len(), 3);
        }
        other => panic!("immutable target must reuse its batch, got {other:?}"),
    }
}

/// `are_same_review_batch_leaves` relaxes the ordinary single-writer chain
/// guard for exactly one case: two leaf executions of the SAME persisted
/// batch. Its false cases are the safety-critical ones — a too-permissive
/// predicate would let genuinely unrelated executions run concurrently on
/// one work item, which is precisely what the guard exists to stop.
#[test]
fn are_same_review_batch_leaves_rejects_cross_batch_and_memberless_pairs() {
    let db = WorkDb::open(temp_db_path("review-batch-leaves-negative")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");

    let (_batch_a, executions_a) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha-a", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };

    // (a) Two leaves belonging to DIFFERENT batches under the same cycle
    // root (a second batch at another target SHA) must not read as "same
    // batch leaves".
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
        !db.are_same_review_batch_leaves(&executions_a[0].id, &executions_b[0].id)
            .unwrap(),
        "leaves from different batches must not be treated as the same batch's leaves"
    );

    // (b) A leaf paired with an execution that has no member row at all
    // must not read as "same batch leaves" either.
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
        !db.are_same_review_batch_leaves(&executions_a[0].id, &bare_execution.id)
            .unwrap(),
        "an execution with no batch member row must not be treated as a batch leaf"
    );
}

/// A batch still live for the current target — one leaf settled with an
/// informative verdict, two still outstanding — must keep reporting
/// `ExistingBatch` for that exact target, never `AlreadyReviewed`. The
/// already-reviewed check runs after the `ExistingBatch` lookup precisely so
/// a live batch always wins over a partial verdict: reporting
/// `AlreadyReviewed` here would tell the caller "this head is settled" while
/// quorum is still pending.
#[test]
fn create_pre_merge_review_batch_prefers_existing_batch_over_a_partial_verdict() {
    let db = WorkDb::open(temp_db_path("review-batch-existing-over-already-reviewed")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge);

    let batch = match db
        .create_pre_merge_review_batch(input.clone(), "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, .. } => batch,
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };

    // One leaf settles with an informative verdict for this exact target —
    // recorded against the cycle root, as a real batch leaf's verdict is —
    // while the batch itself is still non-terminal (two leaves outstanding).
    let leaf_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    WorkDb::insert_review_verdict_in_tx(
        &db.connect().unwrap(),
        &leaf_execution.id,
        &cycle_root.id,
        &ReviewVerdictInput {
            head_sha: Some("head-sha".to_owned()),
            findings_count: 0,
            revision_warranted: false,
            gate_outcome: REVIEW_GATE_OUTCOME_COMPLETED_CLEAN,
        },
    )
    .unwrap();

    match db
        .create_pre_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::ExistingBatch { batch: existing, .. } => {
            assert_eq!(existing.id, batch.id, "must reuse the still-live batch for this target");
        }
        other => panic!("a live batch for this exact target must win over a partial verdict, got {other:?}"),
    }
}

/// The flag-off / non-batch path: `create_pre_merge_review_batch` must
/// treat a genuine legacy (non-batch) non-terminal `pr_review` execution as
/// owning the target and refuse to create a batch alongside it: a target
/// must be wholly old-mode or wholly batch-mode, so the two finalizers can
/// never both act on it. (See docs/designs/multi-agent-code-review.md,
/// "Dispatch three executions, not one execution with subagents".)
#[test]
fn create_pre_merge_review_batch_defers_to_a_genuine_legacy_execution() {
    let db = WorkDb::open(temp_db_path("review-batch-legacy-execution")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");

    let legacy_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();

    match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::LegacyExecution(execution) => {
            assert_eq!(execution.id, legacy_execution.id);
        }
        other => panic!("expected LegacyExecution for a genuine legacy reviewer, got {other:?}"),
    }
}

#[test]
fn dead_leaf_retries_only_its_own_role_once() {
    let db = WorkDb::open(temp_db_path("review-batch-role-retry")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let (batch, executions) = match db
        .create_pre_merge_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    let dead = executions[0].clone();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            rusqlite::params![dead.id],
        )
        .unwrap();

    let candidates = db.list_dead_review_batch_member_candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].execution_id, dead.id);

    let RetryDeadReviewBatchMember::Retried(retry) = db.retry_dead_review_batch_member(&dead.id).unwrap() else {
        panic!("one retry is allowed");
    };
    let members = db.review_batch_members(&batch.id).unwrap();
    let dead_member = members
        .iter()
        .find(|member| member.execution_id.as_deref() == Some(dead.id.as_str()))
        .unwrap();
    assert_eq!(dead_member.status, ReviewBatchMemberStatus::Failed);
    let retries = members
        .iter()
        .filter(|member| member.role == dead_member.role)
        .collect::<Vec<_>>();
    assert_eq!(retries.len(), 2, "only the dead role receives a retry");
    assert!(
        retries
            .iter()
            .any(|member| member.execution_id.as_deref() == Some(retry.id.as_str()) && member.attempt == 2)
    );
    for sibling in members.iter().filter(|member| member.role != dead_member.role) {
        assert_eq!(sibling.attempt, 1, "sibling roles are never retried together");
    }

    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'orphaned' WHERE id = ?1",
            rusqlite::params![retry.id],
        )
        .unwrap();
    assert!(matches!(
        db.retry_dead_review_batch_member(&retry.id).unwrap(),
        RetryDeadReviewBatchMember::NotRetried | RetryDeadReviewBatchMember::BatchFailed
    ));
    let exhausted = db.review_batch_member_for_execution(&retry.id).unwrap().unwrap();
    assert_eq!(exhausted.status, ReviewBatchMemberStatus::Failed);
}

fn stamp_execution_status(db: &WorkDb, execution_id: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = ?2 WHERE id = ?1",
            rusqlite::params![execution_id, status],
        )
        .unwrap();
}

fn create_supervising_batch(
    db: &WorkDb,
    attempt: i64,
    member_status: ReviewBatchMemberStatus,
) -> (String, ReviewBatch, String) {
    let product = create_test_product(db);
    let cycle_root = create_test_chore_manual(db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member_with(
                ReviewBatchMemberRole::Supervisor,
                Some(execution.id.clone()),
                attempt,
                member_status,
            )],
        )
        .unwrap();
    force_batch_supervising(db, &batch.id);
    (cycle_root.id, batch, execution.id)
}

/// A supervisor whose execution died without Stop is a recovery candidate,
/// retried once with the same driver/model, and terminal-fails the batch on
/// the second death — the same one-retry bound as a leaf, applied to the
/// supervisor role the leaf path previously skipped.
#[test]
fn dead_supervisor_retries_once_then_fails_the_batch() {
    let db = WorkDb::open(temp_db_path("dead-supervisor-retry")).unwrap();
    let (cycle_root_id, batch, dead_id) = create_supervising_batch(&db, 1, ReviewBatchMemberStatus::Pending);
    stamp_execution_status(&db, &dead_id, "orphaned");

    let candidates = db.list_dead_review_batch_member_candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].execution_id, dead_id);

    let RetryDeadReviewBatchMember::Retried(retry) = db.retry_dead_review_batch_member(&dead_id).unwrap() else {
        panic!("supervisor gets one retry");
    };
    let members = db.review_batch_members(&batch.id).unwrap();
    let dead_member = members
        .iter()
        .find(|member| member.execution_id.as_deref() == Some(dead_id.as_str()))
        .unwrap();
    assert_eq!(dead_member.status, ReviewBatchMemberStatus::Failed);
    assert_eq!(dead_member.role, ReviewBatchMemberRole::Supervisor);
    assert_eq!(dead_member.attempt, 1);
    let retry_member = members
        .iter()
        .find(|member| member.execution_id.as_deref() == Some(retry.id.as_str()))
        .unwrap();
    assert_eq!(retry_member.role, ReviewBatchMemberRole::Supervisor);
    assert_eq!(retry_member.attempt, 2);
    assert_eq!(retry_member.status, ReviewBatchMemberStatus::Pending);
    assert_eq!(retry_member.requested_driver, dead_member.requested_driver);
    assert_eq!(retry_member.resolved_model, dead_member.resolved_model);
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising,
        "a retried supervisor must keep the batch in supervising, not complete it"
    );
    assert!(find_quorum_failed_attention(&db, &cycle_root_id).is_none());

    stamp_execution_status(&db, &retry.id, "orphaned");
    assert!(matches!(
        db.retry_dead_review_batch_member(&retry.id).unwrap(),
        RetryDeadReviewBatchMember::BatchFailed
    ));
    let failed = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(failed.status, ReviewBatchStatus::Failed);
    assert!(failed.completed_at.is_some());
    let attention = find_quorum_failed_attention(&db, &cycle_root_id)
        .expect("exhausted supervisor retry must file pr_review_quorum_failed");
    assert!(attention.title.to_lowercase().contains("supervisor"));
}

/// The last supervisor attempt can die without Stop ever firing (member
/// still pending at attempt 2). That row must still be a candidate so the
/// recovery sweep can fail the batch — otherwise it sits in `supervising`
/// forever.
#[test]
fn pending_supervisor_attempt_two_orphan_is_a_candidate_and_fails_the_batch() {
    let db = WorkDb::open(temp_db_path("dead-supervisor-attempt-2")).unwrap();
    let (cycle_root_id, batch, dead_id) = create_supervising_batch(&db, 2, ReviewBatchMemberStatus::Pending);
    stamp_execution_status(&db, &dead_id, "orphaned");

    let candidates = db.list_dead_review_batch_member_candidates().unwrap();
    assert_eq!(
        candidates.len(),
        1,
        "attempt-2 pending supervisor must be listed so recovery can fail the batch"
    );
    assert_eq!(candidates[0].execution_id, dead_id);

    assert!(matches!(
        db.retry_dead_review_batch_member(&dead_id).unwrap(),
        RetryDeadReviewBatchMember::BatchFailed
    ));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );
    assert!(find_quorum_failed_attention(&db, &cycle_root_id).is_some());
}

/// A still-live supervisor is not a dead-member candidate — recovery must
/// not steal an in-flight collation.
#[test]
fn live_supervisor_is_not_a_dead_member_candidate() {
    let db = WorkDb::open(temp_db_path("live-supervisor-not-candidate")).unwrap();
    let (_cycle_root_id, _batch, execution_id) = create_supervising_batch(&db, 1, ReviewBatchMemberStatus::Running);
    let candidates = db.list_dead_review_batch_member_candidates().unwrap();
    assert!(
        !candidates.iter().any(|c| c.execution_id == execution_id),
        "a running supervisor execution must not be listed for retry"
    );
}

/// If the PR head has moved since the batch froze its target SHA, refuse to
/// retry the supervisor: collating the original leaf reports would present
/// a review of stale code as covering the current PR.
#[test]
fn moved_head_fails_the_supervising_batch_instead_of_retrying() {
    let db = WorkDb::open(temp_db_path("supervisor-moved-head")).unwrap();
    let (cycle_root_id, batch, dead_id) = create_supervising_batch(&db, 1, ReviewBatchMemberStatus::Pending);
    stamp_execution_status(&db, &dead_id, "orphaned");

    assert!(
        !db.fail_review_batch_for_moved_head(&dead_id, "head-sha").unwrap(),
        "matching SHA must not fail the batch"
    );
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising
    );

    assert!(db.fail_review_batch_for_moved_head(&dead_id, "other-head").unwrap());
    let failed = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(failed.status, ReviewBatchStatus::Failed);
    let attention = find_quorum_failed_attention(&db, &cycle_root_id)
        .expect("moved-head failure must file pr_review_quorum_failed");
    assert!(attention.title.to_lowercase().contains("head moved"));
    assert!(attention.body_markdown.contains("head-sha"));
    assert!(attention.body_markdown.contains("other-head"));

    assert!(
        !db.fail_review_batch_for_moved_head(&dead_id, "other-head").unwrap(),
        "a second call against an already-failed batch is a no-op"
    );
}

/// The review pool's 16-unit reservation capacity divided by the four-unit
/// pre-merge weight admits exactly four concurrent pre-merge batches and
/// denies a fifth — the property that stops a fifth wave of leaves from
/// occupying every slot just as earlier batches become ready to collate.
#[test]
fn can_admit_review_batch_allows_four_concurrent_pre_merge_batches_and_denies_a_fifth() {
    let db = WorkDb::open(temp_db_path("review-pool-admission-pre-merge")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    for i in 0..4 {
        assert!(
            db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
            "batch {i} should still be admitted below the four-batch cap"
        );
        db.create_review_batch(
            batch_input(
                cycle_root.id.clone(),
                &format!("head-sha-{i}"),
                ReviewBatchPhase::PreMerge,
            ),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    }

    assert!(
        !db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
        "a fifth concurrent pre-merge batch must be denied at the 16-unit cap"
    );
}

/// Pre-merge admission is checked against pre-merge reservations alone, so
/// it never has to wait on post-merge safety-net work — this is the
/// design's "pre-merge work has priority over post-merge safety-net work".
#[test]
fn pre_merge_admission_ignores_post_merge_reservations_giving_it_priority() {
    let db = WorkDb::open(temp_db_path("review-pool-admission-priority")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    // Reserve all 16 of the pool's units with post-merge batches alone.
    for i in 0..16 {
        db.create_review_batch(
            post_merge_batch_input(cycle_root.id.clone(), &format!("merge-sha-{i}")),
            &[member(ReviewBatchMemberRole::PostMergeReviewer, None)],
        )
        .unwrap();
    }

    // Post-merge itself is now denied: 16 + 1 > 16.
    assert!(!db.can_admit_review_batch(ReviewBatchPhase::PostMerge).unwrap());

    // But pre-merge is unaffected: its check only looks at pre-merge
    // reservations (currently zero), so it is still admitted even though
    // the pool's raw remaining capacity (0 units) is far below the four
    // units a pre-merge batch reserves.
    assert!(db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap());
}

/// Post-merge admission, unlike pre-merge, is checked against BOTH
/// reservations — it only starts when spare capacity remains after every
/// open pre-merge batch's block.
#[test]
fn post_merge_admission_accounts_for_pre_merge_reservations() {
    let db = WorkDb::open(temp_db_path("review-pool-admission-post-merge")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    for i in 0..4 {
        db.create_review_batch(
            batch_input(
                cycle_root.id.clone(),
                &format!("head-sha-{i}"),
                ReviewBatchPhase::PreMerge,
            ),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    }

    assert!(
        !db.can_admit_review_batch(ReviewBatchPhase::PostMerge).unwrap(),
        "post-merge must be denied once pre-merge batches alone reserve the full 16 units"
    );
}

/// A batch's reservation lasts through its whole non-terminal lifetime and
/// is released only once it reaches `completed` or `failed` — not merely
/// once its leaves have reported.
#[test]
fn a_completed_batch_releases_its_reservation() {
    let db = WorkDb::open(temp_db_path("review-pool-admission-release")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    let mut batches = Vec::new();
    for i in 0..4 {
        let (batch, _) = db
            .create_review_batch(
                batch_input(
                    cycle_root.id.clone(),
                    &format!("head-sha-{i}"),
                    ReviewBatchPhase::PreMerge,
                ),
                &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
            )
            .unwrap();
        batches.push(batch);
    }
    assert!(!db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap());

    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            rusqlite::params![batches[0].id],
        )
        .unwrap();

    assert!(
        db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
        "completing one batch must free its four-unit reservation for a new one"
    );
}

/// The production entry point (`create_pre_merge_review_batch`) defers
/// rather than creating a batch once the pool is at capacity, and leaves no
/// batch or member execution rows behind.
#[test]
fn create_pre_merge_review_batch_defers_when_the_pool_is_at_capacity() {
    let db = WorkDb::open(temp_db_path("review-pool-admission-deferred-dispatch")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    for i in 0..4 {
        db.create_review_batch(
            batch_input(
                cycle_root.id.clone(),
                &format!("head-sha-{i}"),
                ReviewBatchPhase::PreMerge,
            ),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    }

    let deferred_cycle_root = create_test_chore_manual(&db, product.id, "another review target");
    match db
        .create_pre_merge_review_batch(
            batch_input(
                deferred_cycle_root.id.clone(),
                "new-head-sha",
                ReviewBatchPhase::PreMerge,
            ),
            "https://github.com/example/repo",
        )
        .unwrap()
    {
        ReviewBatchDispatch::AdmissionDeferred => {}
        other => panic!("expected AdmissionDeferred once the pool is at capacity, got {other:?}"),
    }
    assert!(
        db.review_batch_for_target(&deferred_cycle_root.id, ReviewBatchPhase::PreMerge, "new-head-sha")
            .unwrap()
            .is_none(),
        "a deferred admission must not create a batch row"
    );
}

/// Admission capacity tracks the configured review-pool size, not the
/// compile-time hard cap: a 4-slot pool admits one four-unit pre-merge
/// batch and denies a second, so leaves cannot starve their supervisor.
#[test]
fn admission_capacity_follows_the_configured_review_pool_size() {
    let db = WorkDb::open(temp_db_path("review-pool-configured-size")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");

    assert!(
        db.can_admit_review_batch_for_pool(ReviewBatchPhase::PreMerge, 4)
            .unwrap()
    );
    db.create_review_batch(
        batch_input(cycle_root.id.clone(), "head-sha-0", ReviewBatchPhase::PreMerge),
        &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
    )
    .unwrap();
    assert!(
        !db.can_admit_review_batch_for_pool(ReviewBatchPhase::PreMerge, 4)
            .unwrap(),
        "a 4-slot pool is filled by one pre-merge batch"
    );
}

/// A `BOSS_REVIEW_POOL_SIZE` below the reservation weight of one pre-merge
/// batch must not deadlock admission: capacity is floored so an empty pool
/// can still admit its first batch regardless of how small the configured
/// pool is.
#[test]
fn admission_capacity_is_floored_so_a_small_pool_still_admits_on_an_empty_pool() {
    let db = WorkDb::open(temp_db_path("review-pool-small-configured-size")).unwrap();
    for pool_size in [1_usize, 2, 3] {
        assert!(
            db.can_admit_review_batch_for_pool(ReviewBatchPhase::PreMerge, pool_size)
                .unwrap(),
            "pool size {pool_size} must admit the first batch on an empty pool rather than deferring forever"
        );
    }
}

/// A batch whose cycle root is deleted or already terminal does not hold a
/// reservation, so it cannot block every other product's pre-merge review.
#[test]
fn reserved_count_excludes_batches_whose_cycle_root_is_gone() {
    let db = WorkDb::open(temp_db_path("review-pool-dead-root")).unwrap();
    let product = create_test_product(&db);
    let mut occupying = Vec::new();
    for i in 0..4 {
        let cycle_root = create_test_chore_manual(&db, product.id.clone(), format!("occupant {i}"));
        let (batch, _) = db
            .create_review_batch(
                batch_input(
                    cycle_root.id.clone(),
                    &format!("head-sha-{i}"),
                    ReviewBatchPhase::PreMerge,
                ),
                &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
            )
            .unwrap();
        occupying.push((cycle_root.id, batch.id));
    }
    assert!(!db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap());

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            rusqlite::params![occupying[0].0, now_string()],
        )
        .unwrap();
    assert!(
        db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
        "a deleted cycle root must release its reservation without waiting for quorum"
    );
}

/// The reaper fails a collecting batch whose cycle root is gone so the row
/// itself stops occupying the pool, and files an operator-visible item.
#[test]
fn reap_inert_review_batches_fails_batches_with_a_deleted_cycle_root() {
    let db = WorkDb::open(temp_db_path("review-pool-reap-deleted")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "gone");
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            rusqlite::params![cycle_root.id, now_string()],
        )
        .unwrap();

    let reaped = db
        .reap_inert_review_batches(crate::work::REVIEW_BATCH_STALE_SECS)
        .unwrap();
    assert_eq!(reaped, vec![batch.id.clone()]);
    let stored = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(stored.status, ReviewBatchStatus::Failed);
    assert!(
        db.can_admit_review_batch(ReviewBatchPhase::PreMerge).unwrap(),
        "reaping a deleted-root batch must release its reservation"
    );
}

/// Reaping a batch whose cycle root is gone must terminalize any leaf still
/// physically running, not just release the reservation — otherwise up to
/// four review-pool slots stay occupied by a batch admission no longer
/// counts against, and the leaf later settles silently against a `failed`
/// batch.
#[tokio::test]
async fn reap_inert_review_batches_terminalizes_running_members_of_a_deleted_cycle_root() {
    let db = WorkDb::open(temp_db_path("review-pool-reap-deleted-frees-slots")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "gone-but-leaves-running");
    let running_execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Running)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(
                ReviewBatchMemberRole::ClaudeReviewer,
                Some(running_execution.id.clone()),
            )],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            rusqlite::params![cycle_root.id, now_string()],
        )
        .unwrap();

    let mut sub = db.event_bus().subscribe(boss_event_bus::TopicFilter::kind(
        boss_event_bus::EventKind::ExecutionTerminal,
    ));

    let reaped = db
        .reap_inert_review_batches(crate::work::REVIEW_BATCH_STALE_SECS)
        .unwrap();
    assert_eq!(reaped, vec![batch.id.clone()]);

    let event = sub
        .recv()
        .await
        .expect("ExecutionTerminal must be published for the leaf the reaper abandons");
    assert_eq!(
        event,
        boss_event_bus::Event::ExecutionTerminal {
            execution_id: running_execution.id.clone(),
            task_id: cycle_root.id.clone(),
            host_id: "local".to_owned(),
            pool_claim: None,
        },
        "the reaper's raw abandon UPDATE must go through the same execution-terminal publish \
         path as every other terminalizing writer, so the pane/worker teardown listening for \
         that event actually runs"
    );

    let terminalized = db.get_execution(&running_execution.id).unwrap();
    assert_eq!(
        terminalized.status,
        ExecutionStatus::Abandoned,
        "a leaf still running when its cycle root disappears must be terminalized so its \
         review-pool slot is actually freed"
    );
    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(
        members[0].status,
        ReviewBatchMemberStatus::Failed,
        "the member row must be marked failed alongside its execution"
    );
}

/// A still-visible terminal cycle root gets an attention item when its
/// wedged batch is reaped.
#[test]
fn reap_inert_review_batches_files_attention_on_a_done_cycle_root() {
    let db = WorkDb::open(temp_db_path("review-pool-reap-done")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "done");
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
            rusqlite::params![cycle_root.id],
        )
        .unwrap();

    let reaped = db
        .reap_inert_review_batches(crate::work::REVIEW_BATCH_STALE_SECS)
        .unwrap();
    assert_eq!(reaped, vec![batch.id.clone()]);
    let attentions = db.list_attention_items_for_work_item(&cycle_root.id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|item| item.kind == crate::work::PR_REVIEW_BATCH_STALE_ATTENTION_KIND),
        "reaping a still-visible cycle root must file an attention item; got {attentions:?}"
    );
}

/// [`WorkDb::create_post_merge_review_batch`] dispatches exactly one
/// `PostMergeReviewer` execution at `large` effort (Opus/high — the task's
/// dispatch policy for a solo landed-tree review), and is idempotent on the
/// same `(cycle_root_id, PostMerge, merge_sha)` target — a redundant call
/// (a racing or retried merge-poller trigger) must read back the existing
/// batch rather than mint a second one.
#[test]
fn create_post_merge_review_batch_dispatches_the_sole_member_and_is_idempotent() {
    let db = WorkDb::open(temp_db_path("post-merge-batch-dispatch")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = post_merge_batch_input(cycle_root.id.clone(), "merge-sha-1");

    let (batch, executions) = match db
        .create_post_merge_review_batch(input.clone(), "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::Created { batch, executions } => (batch, executions),
        other => panic!("expected a newly-created review batch, got {other:?}"),
    };
    assert_eq!(batch.phase, ReviewBatchPhase::PostMerge);
    assert_eq!(batch.merge_sha.as_deref(), Some("merge-sha-1"));
    assert_eq!(batch.target_sha, "merge-sha-1");
    assert_eq!(executions.len(), 1);
    assert_eq!(executions[0].status, ExecutionStatus::Ready);

    let members = db.review_batch_members(&batch.id).unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].role, ReviewBatchMemberRole::PostMergeReviewer);
    assert_eq!(members[0].requested_driver, "claude");
    assert_eq!(members[0].provider_effort, "large");
    assert_eq!(members[0].execution_id.as_deref(), Some(executions[0].id.as_str()));

    match db
        .create_post_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap()
    {
        ReviewBatchDispatch::ExistingBatch {
            batch: existing,
            executions: existing_executions,
        } => {
            assert_eq!(existing.id, batch.id);
            assert_eq!(existing_executions.len(), 1);
            assert_eq!(existing_executions[0].id, executions[0].id);
        }
        other => panic!("expected the existing batch to be read back, got {other:?}"),
    }
}

/// [`WorkDb::create_post_merge_review_batch`] only accepts `post_merge`
/// batches — it is a distinct dispatch path from
/// [`WorkDb::create_pre_merge_review_batch`], not a superset.
#[test]
fn create_post_merge_review_batch_rejects_pre_merge_phase() {
    let db = WorkDb::open(temp_db_path("post-merge-batch-wrong-phase")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = batch_input(cycle_root.id, "head-sha", ReviewBatchPhase::PreMerge);

    let error = db
        .create_post_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap_err();
    assert!(error.to_string().contains("post_merge"));
}

/// A `post_merge` batch is keyed on the merge commit SHA: `target_sha` and
/// `merge_sha` must match. [`validate_batch_input`] enforces this rather than
/// merely documenting it — a caller passing different values is rejected,
/// not silently accepted.
#[test]
fn create_post_merge_review_batch_rejects_target_sha_merge_sha_mismatch() {
    let db = WorkDb::open(temp_db_path("post-merge-batch-sha-mismatch")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let input = ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root.id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PostMerge)
        .pr_number(42)
        .pr_url("https://github.com/example/repo/pull/42")
        .target_sha("target-sha")
        .merge_sha("different-merge-sha")
        .build();

    let error = db
        .create_post_merge_review_batch(input, "https://github.com/example/repo")
        .unwrap_err();
    assert!(
        error.to_string().contains("target_sha == merge_sha"),
        "error must name the violated invariant: {error}"
    );
}

/// A post-merge batch has no leaf/supervisor split: its sole
/// `PostMergeReviewer` member submits the verdict directly while the batch
/// is still `collecting` (the phase never reaches `supervising`), and that
/// single report IS the batch's verdict — quorum moves straight from
/// `collecting` to `applying` on it, unlike the two-of-three leaf gate.
#[test]
fn post_merge_verdict_stages_proposed_and_moves_the_batch_straight_to_applying() {
    let db = WorkDb::open(temp_db_path("post-merge-verdict-proposal")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            post_merge_batch_input(cycle_root.id.clone(), "merge-sha-1"),
            &[member(
                ReviewBatchMemberRole::PostMergeReviewer,
                Some(execution.id.clone()),
            )],
        )
        .unwrap();
    assert_eq!(batch.status, ReviewBatchStatus::Collecting);

    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &post_merge_verdict_payload(&batch.id, "merge-sha-1"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    assert_eq!(outcome.proposal.state, ProposalState::Proposed);
    let applying = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(applying.status, ReviewBatchStatus::Applying);
    assert_eq!(
        applying.final_verdict_proposal_id.as_deref(),
        Some(outcome.proposal.id.as_str())
    );

    let member = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::PostMergeReviewer)
        .unwrap();
    assert_eq!(member.status, ReviewBatchMemberStatus::Reported);
    assert_eq!(member.report_proposal_id.as_deref(), Some(outcome.proposal.id.as_str()));
}

/// A `Supervisor`-only verdict submission is rejected against a post-merge
/// batch's `PostMergeReviewer` member, and vice versa — `apply_review_verdict`
/// gates on the role actually entitled to submit for the batch's phase, not
/// merely on `ProposalKind::ReviewVerdict`.
#[test]
fn post_merge_verdict_is_rejected_from_a_role_not_entitled_to_submit_one() {
    let db = WorkDb::open(temp_db_path("post-merge-verdict-wrong-role")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    // A `ClaudeReviewer` leaf member on a (synthetic, test-only) batch never
    // submits a verdict — only `Supervisor` (pre-merge) or `PostMergeReviewer`
    // (post-merge) may.
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            &[member(
                ReviewBatchMemberRole::ClaudeReviewer,
                Some(execution.id.clone()),
            )],
        )
        .unwrap();

    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    assert_eq!(outcome.proposal.state, ProposalState::Rejected);
    assert!(
        outcome
            .proposal
            .decision_reason
            .as_deref()
            .unwrap_or_default()
            .contains("never submits a review-verdict directly")
    );
}

/// A dead post-merge reviewer (host death before Stop) is retried once with
/// the same driver/model/effort, then terminal-fails the batch on the
/// second death — the same one-retry bound the supervisor gets, applied to
/// the solo post-merge topology. The candidate query must find it even
/// though its owning task is already `done` (unlike the pre-merge query,
/// which explicitly excludes `done`/`archived` tasks).
#[test]
fn dead_post_merge_reviewer_retries_once_then_fails_the_batch() {
    let db = WorkDb::open(temp_db_path("dead-post-merge-reviewer-retry")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let execution = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            post_merge_batch_input(cycle_root.id.clone(), "merge-sha-1"),
            &[member(
                ReviewBatchMemberRole::PostMergeReviewer,
                Some(execution.id.clone()),
            )],
        )
        .unwrap();
    // The owning task is `done` in the real flow (the post-merge batch only
    // exists because the PR already merged) — mirror that here rather than
    // leaving it in whatever `create_test_chore_manual` defaults to, since
    // that is exactly the condition the post-merge candidate query must not
    // filter out.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done' WHERE id = ?1",
            rusqlite::params![cycle_root.id],
        )
        .unwrap();
    stamp_execution_status(&db, &execution.id, "orphaned");

    let candidates = db.list_dead_post_merge_review_batch_member_candidates().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].execution_id, execution.id);
    assert!(
        db.list_dead_review_batch_member_candidates().unwrap().is_empty(),
        "the pre-merge candidate query must never pick up a post-merge member"
    );

    let RetryDeadReviewBatchMember::Retried(retry) = db.retry_dead_review_batch_member(&execution.id).unwrap() else {
        panic!("post-merge reviewer gets one retry");
    };
    let members = db.review_batch_members(&batch.id).unwrap();
    let retry_member = members
        .iter()
        .find(|member| member.execution_id.as_deref() == Some(retry.id.as_str()))
        .unwrap();
    assert_eq!(retry_member.role, ReviewBatchMemberRole::PostMergeReviewer);
    assert_eq!(retry_member.attempt, 2);
    // The retry inherits the dead member's own persisted policy — the test's
    // `member()` fixture uses `medium`, not the `large` effort
    // `create_post_merge_review_batch` actually dispatches with (covered by
    // `create_post_merge_review_batch_dispatches_the_sole_member_and_is_idempotent`).
    assert_eq!(retry_member.provider_effort, "medium");
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Collecting,
        "a retried post-merge reviewer must keep the batch collecting, not complete it"
    );

    stamp_execution_status(&db, &retry.id, "orphaned");
    assert!(matches!(
        db.retry_dead_review_batch_member(&retry.id).unwrap(),
        RetryDeadReviewBatchMember::BatchFailed
    ));
    let failed = db.review_batch(&batch.id).unwrap().unwrap();
    assert_eq!(failed.status, ReviewBatchStatus::Failed);
    let attention = find_quorum_failed_attention(&db, &cycle_root.id)
        .expect("exhausted post-merge reviewer retry must file pr_review_quorum_failed");
    assert!(attention.title.to_lowercase().contains("post-merge"));
}
