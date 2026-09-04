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

/// incident-002 postmortem hold: a cycle root the deletion tripwire halted,
/// pending explicit operator sign-off (`WorkerPrCompletionTarget::
/// BlockedDeletionSignoff` / `conflict_ladder.rs`).
fn bind_blocked_deletion_signoff(db: &WorkDb, task_id: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'deletion_signoff', pr_url = ?2 WHERE id = ?1",
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

/// Seed `tasks.last_reviewed_sha` directly, modelling the SHA a prior
/// PreMerge verdict already stamped via `commit_applied_review_verdict`
/// (`review_verdict_apply.rs::commit_applied_review_verdict` ->
/// `increment_review_cycle_once_in_tx`). Used to construct the exact
/// duplicate-head condition a `PostMerge` verdict must be exempt from.
fn seed_last_reviewed_sha(db: &WorkDb, task_id: &str, sha: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET last_reviewed_sha = ?2 WHERE id = ?1",
            rusqlite::params![task_id, sha],
        )
        .unwrap();
}

fn post_merge_batch_input(cycle_root_id: String, merge_sha: &str) -> ReviewBatchCreateInput {
    ReviewBatchCreateInput::builder()
        .cycle_root_id(cycle_root_id)
        .base_sha("base-sha")
        .classification(classification())
        .phase(ReviewBatchPhase::PostMerge)
        .pr_number(42)
        .pr_url(PR_URL)
        .target_sha(merge_sha)
        .merge_sha(merge_sha)
        .build()
}

fn post_merge_findings_verdict_payload(batch_id: &str, target_sha: &str) -> String {
    format!(
        r#"{{"batch_id":"{batch_id}","verdict":{{"batch_id":"{batch_id}","pr_url":"{PR_URL}","target_sha":"{target_sha}","phase":"post_merge","summary":"One high-severity defect.","revision_warranted":true,"findings":[{{"severity":"high","category":"correctness","confidence":"high","file":"src/lib.rs","title":"Unchecked index","detail":"Out of bounds read.","sources":["claude"]}}],"contradictions":[]}}}}"#
    )
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

/// incident-002 postmortem gate: a cycle root held `blocked:
/// deletion_signoff` must not be silently advanced to `in_review` (with the
/// hold erased) by the async verdict-apply path — that hold is an explicit
/// operator sign-off gate, and only a human clearing it should move the task
/// on. See `advance_cycle_root_to_in_review_in_tx`.
#[test]
fn clean_verdict_does_not_release_a_deletion_signoff_hold() {
    let db = WorkDb::open(temp_db_path("verdict-apply-deletion-signoff")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_blocked_deletion_signoff(&db, &cycle_root.id);
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
    assert_eq!(
        after.status,
        TaskStatus::Blocked,
        "a deletion_signoff hold must survive verdict apply"
    );
    assert_eq!(after.blocked_reason.as_deref(), Some("deletion_signoff"));
    // Bookkeeping (verdict row, review cycle, proposal state) still lands —
    // only the status advance/hold-clear is suppressed.
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    let (cycle, sha) = db.get_task_review_cycle_state(&cycle_root.id).unwrap();
    assert_eq!(cycle, 1);
    assert_eq!(sha.as_deref(), Some("head-sha"));
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

/// Stage one clean verdict and one findings-warranting verdict (each its own
/// cycle root / batch / proposal), plus one proposal already `applied`
/// before the sweep runs, then drive the sweep entry point
/// (`apply_pending_review_verdicts`) directly — the surface both
/// `review_verdict_apply_sweep::run_one_pass` and the crash-recovery path
/// actually call, and which every other test in this file bypasses by
/// calling `apply_review_verdict_proposal` on a known proposal id. Pins the
/// listing predicate (`kind = 'review_verdict' AND state = 'proposed'`,
/// which must skip the already-applied proposal) and the stats accounting
/// (`applied` must count only proposals this pass actually applied, not
/// every proposal it looked at — see the `Ok(None)` note on
/// `apply_pending_review_verdicts`).
#[test]
fn sweep_entry_point_applies_every_proposed_verdict_and_skips_already_applied_ones() {
    let db = WorkDb::open(temp_db_path("verdict-apply-sweep")).unwrap();
    let product = create_test_product(&db);

    // Batch 1: clean verdict, no findings, no revision.
    let clean_root = create_test_chore_manual(&db, product.id.clone(), "clean review target");
    bind_open_pr(&db, &clean_root.id);
    let clean_supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(clean_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (clean_batch, _) = db
        .create_review_batch(
            batch_input(clean_root.id.clone(), "clean-head-sha"),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(clean_supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &clean_batch.id);
    let clean_outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &clean_supervisor.id,
            work_item_id: &clean_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(&clean_batch.id, "clean-head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(clean_outcome.proposal.state, ProposalState::Proposed);

    // Batch 2: findings verdict on an open origin, must mint a revision.
    let findings_root = create_test_chore_manual(&db, product.id.clone(), "findings review target");
    bind_open_pr(&db, &findings_root.id);
    let findings_supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(findings_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let findings_claude = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(findings_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    let (findings_batch, _) = db
        .create_review_batch(
            batch_input(findings_root.id.clone(), "findings-head-sha"),
            &[
                member(
                    ReviewBatchMemberRole::ClaudeReviewer,
                    Some(findings_claude.id),
                    ReviewBatchMemberStatus::Reported,
                ),
                member(
                    ReviewBatchMemberRole::Supervisor,
                    Some(findings_supervisor.id.clone()),
                    ReviewBatchMemberStatus::Pending,
                ),
            ],
        )
        .unwrap();
    force_batch_supervising(&db, &findings_batch.id);
    let findings_outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &findings_supervisor.id,
            work_item_id: &findings_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &findings_verdict_payload(&findings_batch.id, "findings-head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(findings_outcome.proposal.state, ProposalState::Proposed);

    // Batch 3: already applied before the sweep runs — must not be
    // re-counted or re-materialised by the sweep.
    let applied_root = create_test_chore_manual(&db, product.id, "already applied review target");
    bind_open_pr(&db, &applied_root.id);
    let applied_supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(applied_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (applied_batch, _) = db
        .create_review_batch(
            batch_input(applied_root.id.clone(), "applied-head-sha"),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(applied_supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &applied_batch.id);
    let applied_outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &applied_supervisor.id,
            work_item_id: &applied_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(&applied_batch.id, "applied-head-sha"),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    db.apply_review_verdict_proposal(
        &applied_outcome.proposal.id,
        &FakePrStateChecker::always(PrOpenState::Open),
    )
    .unwrap();
    let applied_proposals = db
        .list_worker_proposals(
            None,
            Some(&applied_root.id),
            Some(ProposalKind::ReviewVerdict),
            Some(ProposalState::Applied),
            None,
        )
        .unwrap();
    assert_eq!(
        applied_proposals.len(),
        1,
        "batch 3's proposal must already be applied before the sweep runs"
    );

    let stats = db
        .apply_pending_review_verdicts(&FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(
        stats,
        ReviewVerdictApplyStats {
            applied: 2,
            failed: 0,
            created_work: 1,
            superseded: 0,
        }
    );

    assert_eq!(
        db.review_batch(&clean_batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    assert_eq!(
        db.review_batch(&findings_batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    assert_eq!(
        db.review_batch(&applied_batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed,
        "already-applied batch was completed before the sweep ran and stays completed"
    );
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

struct DeletingPrChecker {
    inner: FakePrStateChecker,
    deletions: Vec<String>,
}

impl PrStateChecker for DeletingPrChecker {
    fn check(&self, pr_url: &str) -> anyhow::Result<PrOpenState> {
        self.inner.check(pr_url)
    }

    fn merged_parent_deletions(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _head_after: &str,
    ) -> Vec<String> {
        self.deletions.clone()
    }
}

fn stage_clean_verdict(db: &WorkDb, cycle_root_id: &str, target_sha: &str) -> (String, String) {
    stage_clean_verdict_with_key(db, cycle_root_id, target_sha, "verdict-1")
}

fn stage_clean_verdict_with_key(
    db: &WorkDb,
    cycle_root_id: &str,
    target_sha: &str,
    idempotency_key: &str,
) -> (String, String) {
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root_id.to_owned(), target_sha),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(db, &batch.id);
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: cycle_root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(&batch.id, target_sha),
            idempotency_key,
        })
        .unwrap()
        .unwrap();
    (batch.id, outcome.proposal.id)
}

fn stage_findings_verdict(db: &WorkDb, cycle_root_id: &str, target_sha: &str) -> (String, String) {
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let claude = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(cycle_root_id.to_owned())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(cycle_root_id.to_owned(), target_sha),
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
    force_batch_supervising(db, &batch.id);
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: cycle_root_id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &findings_verdict_payload(&batch.id, target_sha),
            idempotency_key: "verdict-1",
        })
        .unwrap()
        .unwrap();
    (batch.id, outcome.proposal.id)
}

fn seed_succeeded_conflict_resolution(db: &WorkDb, product_id: String, cycle_root_id: &str) {
    db.mark_chore_blocked_merge_conflict(cycle_root_id, PR_URL).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id,
            work_item_id: cycle_root_id.to_owned(),
            pr_url: PR_URL.to_owned(),
            pr_number: 42,
            head_branch: "feature".to_owned(),
            base_branch: "main".to_owned(),
            base_sha_at_trigger: Some("base-sha".to_owned()),
            head_sha_before: Some("head-before".to_owned()),
        })
        .unwrap()
        .unwrap();
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("head-sha"))
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', blocked_reason = NULL, pr_url = ?2 WHERE id = ?1",
            rusqlite::params![cycle_root_id, PR_URL],
        )
        .unwrap();
}

/// Stages a corrected clean verdict from the same supervisor during the
/// tripwire probe, so the in-flight proposal is superseded before
/// `commit_applied_review_verdict` re-reads its state.
struct ResubmitOnTripwireCheck {
    inner: FakePrStateChecker,
    db: WorkDb,
    execution_id: String,
    work_item_id: String,
    payload_json: String,
}

impl PrStateChecker for ResubmitOnTripwireCheck {
    fn check(&self, pr_url: &str) -> anyhow::Result<PrOpenState> {
        self.inner.check(pr_url)
    }

    fn merged_parent_deletions(
        &self,
        _repo_slug: &str,
        _head_before: &str,
        _base_sha: &str,
        _head_after: &str,
    ) -> Vec<String> {
        self.db
            .submit_worker_proposal(SubmitWorkerProposalInput {
                execution_id: &self.execution_id,
                work_item_id: &self.work_item_id,
                kind: ProposalKind::ReviewVerdict,
                payload_json: &self.payload_json,
                idempotency_key: "verdict-corrected",
            })
            .expect("corrected resubmission must store")
            .expect("corrected resubmission must stage");
        Vec::new()
    }
}

#[test]
fn tombstone_cancels_the_orphaned_revision_ready_execution() {
    let db = WorkDb::open(temp_db_path("verdict-apply-tombstone-cancel")).unwrap();
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
    let created = db
        .apply_review_verdict_proposal(&outcome.proposal.id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap()
        .expect("findings must mint a revision");
    let before = db.list_executions(Some(&created)).unwrap();
    assert!(
        before.iter().any(|e| e.status == ExecutionStatus::Ready),
        "autostart revision is born with a ready execution"
    );

    db.test_tombstone_orphaned_remediation(&created).unwrap();

    let after_task = query_task(&db.connect().unwrap(), &created).unwrap().unwrap();
    assert!(after_task.deleted_at.is_some());
    let after = db.list_executions(Some(&created)).unwrap();
    assert!(
        after.iter().all(|e| e.status == ExecutionStatus::Cancelled),
        "tombstone must cancel never-started executions so list_ready_executions will not keep draining them"
    );
    assert!(
        db.list_ready_executions()
            .unwrap()
            .iter()
            .all(|e| e.work_item_id != created),
        "cancelled execution must drop out of the ready dispatch queue"
    );
}

#[test]
fn clean_verdict_runs_the_merge_parent_deletion_tripwire() {
    let db = WorkDb::open(temp_db_path("verdict-apply-deletion-tripwire")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");
    bind_open_pr(&db, &cycle_root.id);
    seed_succeeded_conflict_resolution(&db, product.id, &cycle_root.id);

    let (batch_id, proposal_id) = stage_clean_verdict(&db, &cycle_root.id, "head-sha");
    let checker = DeletingPrChecker {
        inner: FakePrStateChecker::always(PrOpenState::Open),
        deletions: vec!["`src/lib.rs` — added by a merged parent, removed by this resolution".to_owned()],
    };
    let created = db.apply_review_verdict_proposal(&proposal_id, &checker).unwrap();
    assert_eq!(created, None, "tripwire must not mint a revision");

    let after = query_task(&db.connect().unwrap(), &cycle_root.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::Blocked);
    assert_eq!(after.blocked_reason.as_deref(), Some("deletion_signoff"));
    assert_eq!(
        db.review_batch(&batch_id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    let attentions = db.list_attention_items_for_work_item(&cycle_root.id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|item| item.kind == crate::merge_parent_deletion::SIGNOFF_ATTENTION_KIND && item.status == "open"),
        "tripwire must file the same sign-off attention as the legacy finalize path"
    );
}

#[test]
fn supervising_batch_is_re_advanced_to_applying_then_applied() {
    let db = WorkDb::open(temp_db_path("verdict-apply-self-heal")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &cycle_root.id);
    let (batch_id, proposal_id) = stage_clean_verdict(&db, &cycle_root.id, "head-sha");
    assert_eq!(
        db.review_batch(&batch_id).unwrap().unwrap().status,
        ReviewBatchStatus::Applying
    );
    force_batch_supervising(&db, &batch_id);

    let created = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(created, None);
    assert_eq!(
        db.review_batch(&batch_id).unwrap().unwrap().status,
        ReviewBatchStatus::Completed
    );
    let after = query_task(&db.connect().unwrap(), &cycle_root.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::InReview);
}

#[test]
fn persistent_not_applying_bail_files_attention_once_and_is_not_counted_applied() {
    let db = WorkDb::open(temp_db_path("verdict-apply-stranded-once")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &cycle_root.id);
    let (batch_id, proposal_id) = stage_clean_verdict(&db, &cycle_root.id, "head-sha");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'failed' WHERE id = ?1",
            rusqlite::params![batch_id],
        )
        .unwrap();

    let created = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(created, None);
    let proposal = db
        .list_worker_proposals(
            None,
            Some(&cycle_root.id),
            Some(ProposalKind::ReviewVerdict),
            Some(ProposalState::Proposed),
            None,
        )
        .unwrap();
    assert_eq!(proposal.len(), 1, "stranded apply must leave the proposal proposed");

    db.apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    let attentions: Vec<_> = db
        .list_attention_items_for_work_item(&cycle_root.id)
        .unwrap()
        .into_iter()
        .filter(|item| item.kind == REVIEW_VERDICT_BATCH_NOT_APPLYING_ATTENTION_KIND)
        .collect();
    assert_eq!(
        attentions.len(),
        1,
        "a persistent not-applying bail must file attention once, not on every sweep pass"
    );

    let stats = db
        .apply_pending_review_verdicts(&FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(
        stats,
        ReviewVerdictApplyStats {
            applied: 0,
            failed: 0,
            created_work: 0,
            superseded: 0,
        }
    );
}

#[test]
fn sweep_does_not_list_an_already_superseded_proposal() {
    let db = WorkDb::open(temp_db_path("verdict-apply-already-superseded")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &cycle_root.id);
    let (_, proposal_id) = stage_clean_verdict(&db, &cycle_root.id, "head-sha");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE worker_proposals SET state = 'superseded' WHERE id = ?1",
            rusqlite::params![proposal_id],
        )
        .unwrap();

    let stats = db
        .apply_pending_review_verdicts(&FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    assert_eq!(
        stats,
        ReviewVerdictApplyStats {
            applied: 0,
            failed: 0,
            created_work: 0,
            superseded: 0,
        },
        "a proposal already superseded is not listed as proposed, so the superseded counter is not touched"
    );
}

#[test]
fn tripwire_retry_tombstones_an_already_materialised_remediation() {
    let db = WorkDb::open(temp_db_path("verdict-apply-tripwire-tombstone")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id.clone(), "review target");
    bind_open_pr(&db, &cycle_root.id);
    seed_succeeded_conflict_resolution(&db, product.id, &cycle_root.id);
    let (batch_id, proposal_id) = stage_findings_verdict(&db, &cycle_root.id, "head-sha");

    let created = db
        .apply_review_verdict_proposal(&proposal_id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap()
        .expect("first apply must mint a revision");
    let before = db.list_executions(Some(&created)).unwrap();
    assert!(
        before.iter().any(|e| e.status == ExecutionStatus::Ready),
        "autostart revision is born with a ready execution"
    );

    // Simulate the retry window: materialisation committed, bookkeeping did not.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE worker_proposals
         SET state = 'proposed', applied_ref = NULL, decided_by = NULL, decided_at = NULL
         WHERE id = ?1",
        rusqlite::params![proposal_id],
    )
    .unwrap();
    conn.execute(
        "DELETE FROM pr_review_verdicts WHERE batch_id = ?1",
        rusqlite::params![batch_id],
    )
    .unwrap();
    conn.execute(
        "UPDATE pr_review_batches SET status = 'applying', completed_at = NULL WHERE id = ?1",
        rusqlite::params![batch_id],
    )
    .unwrap();
    drop(conn);

    let checker = DeletingPrChecker {
        inner: FakePrStateChecker::always(PrOpenState::Open),
        deletions: vec!["`src/lib.rs` — added by a merged parent, removed by this resolution".to_owned()],
    };
    let replay = db.apply_review_verdict_proposal(&proposal_id, &checker).unwrap();
    assert_eq!(replay, None, "tripwire retry must not keep the minted revision");

    let after_task = query_task(&db.connect().unwrap(), &created).unwrap().unwrap();
    assert!(
        after_task.deleted_at.is_some(),
        "already-materialised remediation must be tombstoned when the tripwire fires on retry"
    );
    let after = db.list_executions(Some(&created)).unwrap();
    assert!(
        after.iter().all(|e| e.status == ExecutionStatus::Cancelled),
        "tombstone must cancel never-started executions so the hold is not racing a live worker"
    );
    let after_root = query_task(&db.connect().unwrap(), &cycle_root.id).unwrap().unwrap();
    assert_eq!(after_root.status, TaskStatus::Blocked);
    assert_eq!(after_root.blocked_reason.as_deref(), Some("deletion_signoff"));
    let verdict = db
        .latest_review_verdict(&cycle_root.id)
        .unwrap()
        .expect("retry still records the verdict");
    assert_eq!(
        verdict.revision_task_id, None,
        "held apply must not keep remediating_task_id pointing at the tombstoned revision"
    );
}

#[test]
fn sweep_counts_a_proposal_superseded_during_apply() {
    let db = WorkDb::open(temp_db_path("verdict-apply-supersede-during-apply")).unwrap();
    let product = create_test_product(&db);
    let applied_root = create_test_chore_manual(&db, product.id.clone(), "applied root");
    bind_open_pr(&db, &applied_root.id);
    let superseded_root = create_test_chore_manual(&db, product.id.clone(), "superseded root");
    bind_open_pr(&db, &superseded_root.id);
    seed_succeeded_conflict_resolution(&db, product.id, &superseded_root.id);

    let _ = stage_clean_verdict_with_key(&db, &applied_root.id, "head-sha", "verdict-applied");

    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(superseded_root.id.clone())
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(superseded_root.id.clone(), "head-sha"),
            &[member(
                ReviewBatchMemberRole::Supervisor,
                Some(supervisor.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    force_batch_supervising(&db, &batch.id);
    let first = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &supervisor.id,
            work_item_id: &superseded_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &clean_verdict_payload(&batch.id, "head-sha"),
            idempotency_key: "verdict-first",
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.proposal.state, ProposalState::Proposed);

    let checker = ResubmitOnTripwireCheck {
        inner: FakePrStateChecker::always(PrOpenState::Open),
        db: db.clone(),
        execution_id: supervisor.id.clone(),
        work_item_id: superseded_root.id.clone(),
        payload_json: clean_verdict_payload(&batch.id, "head-sha"),
    };
    let stats = db.apply_pending_review_verdicts(&checker).unwrap();
    assert_eq!(
        stats,
        ReviewVerdictApplyStats {
            applied: 1,
            failed: 0,
            created_work: 0,
            superseded: 1,
        },
        "a proposal still proposed at list time that lands superseded in commit must increment superseded"
    );
}

/// A `PostMerge` verdict must materialise its follow-up even when
/// `verdict.target_sha` equals `tasks.last_reviewed_sha` — the duplicate-head
/// guard exists to suppress a re-review of an unchanged *pre-merge* head, and
/// has no meaning here: a post-merge verdict's `target_sha` is always either
/// the merge commit or (the `head_ref_oid` fallback in
/// `maybe_trigger_post_merge_review`) the PR's own head SHA, which is
/// precisely the SHA the pre-merge verdict already stamped into
/// `last_reviewed_sha`. Before the `batch.phase != PostMerge` exemption in
/// `apply_review_verdict_proposal`, this exact equality tripped
/// `duplicate_head` and silently dropped every post-merge finding with no
/// attention item — this test seeds that same equality and asserts the
/// finding survives instead. One target-sha value stands in for both the
/// merge-SHA and head-SHA-fallback cases named in the review finding: the
/// apply-level guard only ever sees the final `target_sha`, so it cannot
/// distinguish which of the two produced it — the equality-with-prior-head
/// condition is identical either way.
#[test]
fn post_merge_verdict_materialises_a_followup_despite_matching_the_prior_reviewed_head() {
    let db = WorkDb::open(temp_db_path("verdict-apply-post-merge-duplicate-head")).unwrap();
    let product = create_test_product(&db);
    let cycle_root = create_test_chore_manual(&db, product.id, "review target");
    bind_merged_pr(&db, &cycle_root.id);

    let shared_sha = "shared-head-and-merge-sha";
    seed_last_reviewed_sha(&db, &cycle_root.id, shared_sha);

    let post_merge_reviewer = db
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
            post_merge_batch_input(cycle_root.id.clone(), shared_sha),
            &[member(
                ReviewBatchMemberRole::PostMergeReviewer,
                Some(post_merge_reviewer.id.clone()),
                ReviewBatchMemberStatus::Pending,
            )],
        )
        .unwrap();
    // A post-merge batch's sole member moves it straight to `applying` on
    // report — no `force_batch_supervising` needed (unlike the pre-merge
    // supervisor path above).
    let outcome = db
        .submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &post_merge_reviewer.id,
            work_item_id: &cycle_root.id,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &post_merge_findings_verdict_payload(&batch.id, shared_sha),
            idempotency_key: "post-merge-verdict-1",
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Applying
    );

    let created = db
        .apply_review_verdict_proposal(&outcome.proposal.id, &FakePrStateChecker::always(PrOpenState::Merged))
        .unwrap()
        .expect("a post-merge verdict's findings must materialise a follow-up even at the prior reviewed head");
    let task = query_task(&db.connect().unwrap(), &created).unwrap().unwrap();
    assert_eq!(task.kind, TaskKind::Followup);

    let verdict = db
        .latest_review_verdict(&cycle_root.id)
        .unwrap()
        .expect("apply must record the verdict");
    assert_eq!(
        verdict.gate_outcome,
        crate::work::REVIEW_GATE_OUTCOME_COMPLETED_WITH_FINDINGS,
        "gate_outcome must not be dropped_duplicate_head for a PostMerge batch"
    );
    assert_eq!(verdict.revision_task_id.as_deref(), Some(created.as_str()));
}
