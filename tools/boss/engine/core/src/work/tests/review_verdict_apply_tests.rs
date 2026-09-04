//! Asynchronous `review_verdict` application: clean advancement, open-origin
//! revision, merged-origin follow-up, merge-during-apply race, and
//! proposal-id idempotency.

use boss_protocol::{
    ProposalKind, ProposalState, ReviewBatchMemberRole, ReviewBatchMemberStatus, ReviewBatchPhase, ReviewBatchStatus,
    ReviewClassification, ReviewLanguageBucket, ReviewProfile, TaskKind, TaskStatus,
};

use super::*;

const PR_URL: &str = "https://github.com/example/repo/pull/42";

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

fn batch_input(cycle_root_id: String, target_sha: &str) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PreMerge)
        .pr_number(42)
        .pr_url(PR_URL)
        .target_sha(target_sha)
        .build()
}

fn member(
    role: ReviewBatchMemberRole,
    execution_id: Option<String>,
    status: ReviewBatchMemberStatus,
) -> ReviewBatchMemberCreateInput {
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
        .status(status)
        .maybe_execution_id(execution_id)
        .build()
}

fn force_batch_supervising(db: &WorkDb, batch_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'supervising' WHERE id = ?1",
            rusqlite::params![batch_id],
        )
        .unwrap();
}

fn bind_open_pr(db: &WorkDb, task_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, PR_URL],
        )
        .unwrap();
}

fn bind_merged_pr(db: &WorkDb, task_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'done', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![task_id, PR_URL],
        )
        .unwrap();
}

fn clean_verdict_payload(batch_id: &str, target_sha: &str) -> String {
    format!(
        r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"{PR_URL}","target_sha":"{target_sha}","phase":"pre_merge","summary":"Clean.","revision_warranted":false,"findings":[],"contradictions":[]}}}}"#
    )
}

fn findings_verdict_payload(batch_id: &str, target_sha: &str) -> String {
    format!(
        r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"{PR_URL}","target_sha":"{target_sha}","phase":"pre_merge","summary":"One high-severity defect.","revision_warranted":true,"findings":[{{"severity":"high","category":"correctness","confidence":"high","file":"src/lib.rs","title":"Unchecked index","detail":"Out of bounds read.","sources":["claude"]}}],"contradictions":[]}}}}"#
    )
}

#[test]
fn clean_verdict_advances_the_origin_to_review_without_a_revision() {
    let db = WorkDb::open(temp_db_path("verdict-apply-clean")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &cycle_root.id);
    let supervisor = db
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
            batch_input(cycle_root.id.clone(), "head-sha"),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();

    let created = db
        .apply_review_verdict_proposal(&outcome.proposal.id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(created, None);

    let after = query_task(&db.connect().unwrap(), &cycle_root.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
    let (cycle, sha) = db.get_task_review_cycle_state(&cycle_root.id).unwrap();
    assert_eq!(cycle, 1);
    assert_eq!(sha.as_deref(), Some("head-sha"));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    let revision_count: i64 = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE parent_task_id = ?1 AND kind = 'revision'",
            rusqlite::params![cycle_root.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(revision_count, 0, "clean verdict must not mint a revision");
}

#[test]
fn qualifying_findings_on_an_open_origin_create_a_revision() {
    let db = WorkDb::open(temp_db_path("verdict-apply-open-revision")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &cycle_root.id);
    let (batch_id, proposal_id, _) = {
        let supervisor = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let claude = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let (batch, _) = db
            .create_review_batch(
                batch_input(cycle_root.id.clone(), "head-sha"),
                &[
                    member(
                        ReviewBatchMemberRole::ClaudeReviewer,
                        Some(claude.id),
                        ReviewBatchMemberStatus::Reported,
                    ),
                    member(
                        ReviewBatchMemberRole::Supervisor,
                        Some(supervisor.id.clone()),
                        ReviewBatchMemberStatus::Pending,
                    ),
                ],
            )
            .unwrap();
        force_batch_supervising(&db, &batch.id);
        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &supervisor.id,
                work_item_id: &cycle_root.id,
                kind: ProposalKind::ReviewVerdict,
                payload_json: &findings_verdict_payload(&batch.id, "head-sha"),
                idempotency_key: "verdict-1",
            })
            .unwrap()
            .unwrap();
        (batch.id, outcome.proposal.id, supervisor.id)
    };

    let created = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap()
        .expect("open origin with findings must mint a revision");
    let task = query_task(&db.connect().unwrap(), &created).unwrap().unwrap();
    assert_eq!(task.kind, TaskKind::Revision);
    assert!(task.created_via.starts_with(CREATED_VIA_PR_REVIEW_PREFIX));
    assert!(task.description.contains("finalising this revision"));
    assert_eq!(
        db.review_batch(&batch_id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    let (cycle, _) = db.get_task_review_cycle_state(&cycle_root.id).unwrap();
    assert_eq!(cycle, 1);

    let replay = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(replay, None, "an already-applied proposal is a no-op");
    let (cycle_after, _) = db.get_task_review_cycle_state(&cycle_root.id).unwrap();
    assert_eq!(cycle_after, 1, "reapply must not increment the review cycle again");
}

#[test]
fn qualifying_findings_on_a_merged_origin_create_a_followup() {
    let db = WorkDb::open(temp_db_path("verdict-apply-merged-followup")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_merged_pr(&db, &cycle_root.id);
    let (_, proposal_id, _) = {
        let supervisor = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let claude = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let (batch, _) = db
            .create_review_batch(
                batch_input(cycle_root.id.clone(), "head-sha"),
                &[
                    member(
                        ReviewBatchMemberRole::ClaudeReviewer,
                        Some(claude.id),
                        ReviewBatchMemberStatus::Reported,
                    ),
                    member(
                        ReviewBatchMemberRole::Supervisor,
                        Some(supervisor.id.clone()),
                        ReviewBatchMemberStatus::Pending,
                    ),
                ],
            )
            .unwrap();
        force_batch_supervising(&db, &batch.id);
        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &supervisor.id,
                work_item_id: &cycle_root.id,
                kind: ProposalKind::ReviewVerdict,
                payload_json: &findings_verdict_payload(&batch.id, "head-sha"),
                idempotency_key: "verdict-1",
            })
            .unwrap()
            .unwrap();
        (batch.id, outcome.proposal.id, supervisor.id)
    };

    let created = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Merged))
        .unwrap()
        .expect("merged origin with findings must mint a follow-up");
    let task = query_task(&db.connect().unwrap(), &created).unwrap().unwrap();
    assert_eq!(task.kind, TaskKind::Followup);
    assert_eq!(task.origin_pr_number, Some(42));
    assert!(task.description.contains("closing this follow-up"));
    assert!(!task.description.contains("finalising this revision"));
    let parent = query_task(&db.connect().unwrap(), &cycle_root.id).unwrap().unwrap();
    assert_eq!(
        parent.status,
        TaskStatus::Done,
        "merged origin must not be yanked back to review"
    );
}

#[test]
fn merge_during_apply_creates_a_followup_instead_of_discarding_findings() {
    let db = WorkDb::open(temp_db_path("verdict-apply-merge-race")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    // Cached status is still in_review (poller has not flipped it) but the
    // live probe reports Merged — the merge-during-apply race.
    bind_open_pr(&db, &cycle_root.id);
    let (_, proposal_id, _) = {
        let supervisor = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let claude = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let (batch, _) = db
            .create_review_batch(
                batch_input(cycle_root.id.clone(), "head-sha"),
                &[
                    member(
                        ReviewBatchMemberRole::ClaudeReviewer,
                        Some(claude.id),
                        ReviewBatchMemberStatus::Reported,
                    ),
                    member(
                        ReviewBatchMemberRole::Supervisor,
                        Some(supervisor.id.clone()),
                        ReviewBatchMemberStatus::Pending,
                    ),
                ],
            )
            .unwrap();
        force_batch_supervising(&db, &batch.id);
        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &supervisor.id,
                work_item_id: &cycle_root.id,
                kind: ProposalKind::ReviewVerdict,
                payload_json: &findings_verdict_payload(&batch.id, "head-sha"),
                idempotency_key: "verdict-1",
            })
            .unwrap()
            .unwrap();
        (batch.id, outcome.proposal.id, supervisor.id)
    };

    let created = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Merged))
        .unwrap()
        .expect("merge-during-apply must mint a follow-up, not drop findings");
    let task = query_task(&db.connect().unwrap(), &created).unwrap().unwrap();
    assert_eq!(task.kind, TaskKind::Followup);
    assert!(
        db.list_chores(&task.product_id, None, false)
            .unwrap()
            .iter()
            .all(|row| row.kind != TaskKind::Revision),
        "merge-during-apply must not leave a revision row behind"
    );
}

#[test]
fn reapplying_a_merged_followup_returns_the_same_work_item() {
    let db = WorkDb::open(temp_db_path("verdict-apply-followup-idempotent")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_merged_pr(&db, &cycle_root.id);
    let (_, proposal_id, _) = {
        let supervisor = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Ready)
                    .build(),
            )
            .unwrap();
        let claude = db
            .create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(cycle_root.id.clone())
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Completed)
                    .build(),
            )
            .unwrap();
        let (batch, _) = db
            .create_review_batch(
                batch_input(cycle_root.id.clone(), "head-sha"),
                &[
                    member(
                        ReviewBatchMemberRole::ClaudeReviewer,
                        Some(claude.id),
                        ReviewBatchMemberStatus::Reported,
                    ),
                    member(
                        ReviewBatchMemberRole::Supervisor,
                        Some(supervisor.id.clone()),
                        ReviewBatchMemberStatus::Pending,
                    ),
                ],
            )
            .unwrap();
        force_batch_supervising(&db, &batch.id);
        let outcome = db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &supervisor.id,
                work_item_id: &cycle_root.id,
                kind: ProposalKind::ReviewVerdict,
                payload_json: &findings_verdict_payload(&batch.id, "head-sha"),
                idempotency_key: "verdict-1",
            })
            .unwrap()
            .unwrap();
        (batch.id, outcome.proposal.id, supervisor.id)
    };

    let first = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Merged))
        .unwrap()
        .unwrap();
    // Simulate a crash after materialisation but before the proposal was
    // marked applied: reset the proposal to proposed and the batch to applying.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE worker_proposals SET state = 'proposed', applied_ref = NULL, decided_at = NULL, decided_by = NULL WHERE id = ?1",
            rusqlite::params![proposal_id],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'applying', completed_at = NULL WHERE final_verdict_proposal_id = ?1",
            rusqlite::params![proposal_id],
        )
        .unwrap();
    let second = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Merged))
        .unwrap()
        .unwrap();
    assert_eq!(first, second, "proposal id is the materialisation idempotency key");
}

#[test]
fn verdict_citing_an_unreported_role_is_rejected() {
    let db = WorkDb::open(temp_db_path("verdict-unknown-source")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    let supervisor = db
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
            batch_input(cycle_root.id.clone(), "head-sha"),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &findings_verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(outcome.proposal.state, ProposalState::Rejected);
    assert!(
        outcome
            .proposal
            .decision_reason
            .unwrap_or_default()
            .contains("no accepted report")
    );
}
