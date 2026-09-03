//! `pr_review_batches` / `pr_review_batch_members`: immutable target,
//! classification, membership, and uniqueness contracts.

use boss_protocol::{
    ProposalDecider, ProposalKind, ProposalState, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase,
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
fn review_verdict_remains_proposed_for_the_asynchronous_applier() {
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

    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &format!(r#"{{"batch_id":"{}","verdict":{{"outcome":"approved"}}}}"#, batch.id),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    assert_eq!(outcome.proposal.state, ProposalState::Proposed);
    assert_eq!(outcome.proposal.applied_ref, None);
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().final_verdict_proposal_id,
        None
    );
}

#[test]
fn newer_review_verdict_supersedes_the_prior_batch_verdict() {
    let db = WorkDb::open(temp_db_path("review-verdict-supersede")).unwrap();
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
    let submit = |idempotency_key| {
        db.submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &execution.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &format!(r#"{{"batch_id":"{}","verdict":{{"outcome":"approved"}}}}"#, batch.id),
            idempotency_key,
        })
        .unwrap()
        .unwrap()
    };
    let first = submit("verdict-1");
    let second = submit("verdict-2");
    let proposals = db
        .list_worker_proposals_for_work_item(&cycle_root.id, Some(ProposalKind::ReviewVerdict), None)
        .unwrap();
    let first = proposals
        .iter()
        .find(|proposal| proposal.id == first.proposal.id)
        .unwrap();
    assert_eq!(first.state, ProposalState::Superseded);
    assert_eq!(second.proposal.state, ProposalState::Proposed);
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

    let retry = db
        .retry_dead_review_batch_member(&dead.id)
        .unwrap()
        .expect("one retry is allowed");
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
    assert!(db.retry_dead_review_batch_member(&retry.id).unwrap().is_none());
    let exhausted = db.review_batch_member_for_execution(&retry.id).unwrap().unwrap();
    assert_eq!(exhausted.status, ReviewBatchMemberStatus::Failed);
}
