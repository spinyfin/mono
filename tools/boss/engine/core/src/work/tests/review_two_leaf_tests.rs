//! New two-leaf topology, including the proposal submission boundary.
use super::*;

#[test]
fn both_reports_dispatch_supervisor_and_grok_citations_are_rejected() {
    for profile in [ReviewProfile::Light, ReviewProfile::Standard, ReviewProfile::Deep] {
        let db = WorkDb::open(temp_db_path("two-leaf-verdict")).unwrap();
        let product = create_test_product(&db);
        let root = create_test_chore_manual(&db, product.id, "review target");
        let mut input = batch_input(root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge);
        input.classification.profile = profile;
        let ReviewBatchDispatch::Created { batch, executions } = db
            .create_pre_merge_review_batch(input, "https://github.com/example/repo")
            .unwrap()
        else {
            panic!("expected new batch")
        };
        let members = db.review_batch_members(&batch.id).unwrap();
        assert_eq!(
            members.iter().map(|member| member.role).collect::<Vec<_>>(),
            vec![
                ReviewBatchMemberRole::ClaudeReviewer,
                ReviewBatchMemberRole::CodexReviewer
            ]
        );
        assert_eq!(executions.len(), 2);
        for (index, execution) in executions.iter().enumerate() {
            assert_eq!(
                submit_review_report(&db, &execution.id, &root.id, &batch.id, "head-sha", "report")
                    .proposal
                    .state,
                ProposalState::Applied
            );
            assert_eq!(
                db.review_batch(&batch.id).unwrap().unwrap().status,
                if index == 0 {
                    ReviewBatchStatus::Collecting
                } else {
                    ReviewBatchStatus::Supervising
                }
            );
        }
        let supervisor = db
            .review_batch_members(&batch.id)
            .unwrap()
            .into_iter()
            .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
            .unwrap();
        for (index, citation) in ["source", "position", "winner"].into_iter().enumerate() {
            let mut payload: serde_json::Value = serde_json::from_str(&verdict_payload(&batch.id, "head-sha")).unwrap();
            if citation == "source" {
                payload["verdict"]["findings"] = serde_json::json!([{
                    "id": "C1", "severity": "high", "category": "correctness", "confidence": "high",
                    "file": "src/lib.rs", "title": "Unchecked index", "detail": "Missing bounds check",
                    "sources": ["grok-1"]
                }]);
            } else {
                payload["verdict"]["contradictions"] = serde_json::json!([{
                    "file": "src/lib.rs", "description": "Disagreement", "resolution": "Checked source",
                    "positions": [{"role": "claude", "claim": "safe"},
                        {"role": if citation == "position" { "grok" } else { "codex" }, "claim": "unsafe"}],
                    "resolved_in_favor_of": "grok"
                }]);
            }
            let result = db
                .submit_worker_proposal(SubmitWorkerProposalInput {
                    execution_id: supervisor.execution_id.as_deref().unwrap(),
                    work_item_id: &root.id,
                    kind: ProposalKind::ReviewVerdict,
                    payload_json: &payload.to_string(),
                    idempotency_key: &format!("invalid-{index}"),
                })
                .unwrap();
            let proposal = result.unwrap().proposal;
            assert_eq!(proposal.state, ProposalState::Rejected);
            assert!(proposal.decision_reason.unwrap().contains("no accepted report"));
        }
        assert_eq!(
            db.review_batch(&batch.id).unwrap().unwrap().status,
            ReviewBatchStatus::Supervising
        );
    }
}

#[test]
fn new_batch_fails_if_either_leaf_exhausts_its_retry() {
    let db = WorkDb::open(temp_db_path("two-leaf-insufficient")).unwrap();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "review target");
    let (batch, _) = db
        .create_review_batch(
            batch_input(root.id, "head-sha", ReviewBatchPhase::PreMerge),
            &[
                member_with(
                    ReviewBatchMemberRole::ClaudeReviewer,
                    None,
                    1,
                    ReviewBatchMemberStatus::Reported,
                ),
                member_with(
                    ReviewBatchMemberRole::CodexReviewer,
                    None,
                    2,
                    ReviewBatchMemberStatus::Failed,
                ),
            ],
        )
        .unwrap();
    assert!(matches!(
        advance_quorum(&db, &batch.id),
        ReviewBatchQuorumOutcome::InsufficientQuorum
    ));
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );
}

fn review_card(db: &WorkDb, product_id: &str, chore_id: &str) -> boss_protocol::Task {
    db.get_work_tree(product_id)
        .unwrap()
        .chores
        .into_iter()
        .find(|chore| chore.id == chore_id)
        .expect("chore present in work tree")
}

fn start_review_run(db: &WorkDb, execution_id: &str, slot: usize) -> boss_protocol::WorkRun {
    db.start_execution_run(
        execution_id,
        &format!("worker-{slot}"),
        "mono",
        &format!("lease-{slot}"),
        &format!("mono-agent-{slot:03}"),
        &format!("/tmp/mono-agent-{slot:03}"),
    )
    .unwrap()
    .1
}

/// The card and the live-worker metadata report the review stage the engine
/// recorded at launch: the parallel reviewers first, then the consolidating
/// supervisor — never the supervisor as one more reviewer.
#[test]
fn review_stage_reports_reviewers_then_consolidation() {
    let db = WorkDb::open(temp_db_path("review-stage")).unwrap();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "review target");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review', pr_url = 'https://github.com/example/repo/pull/42' WHERE id = ?1",
            rusqlite::params![chore.id],
        )
        .unwrap();
    let ReviewBatchDispatch::Created { batch, executions } = db
        .create_pre_merge_review_batch(
            batch_input(chore.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    else {
        panic!("expected new batch")
    };
    let runs: Vec<_> = executions
        .iter()
        .enumerate()
        .map(|(slot, execution)| start_review_run(&db, &execution.id, slot))
        .collect();

    // Parallel reviewers running: the plain "reviewing" presentation.
    let card = review_card(&db, &product.id, &chore.id);
    assert!(card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));
    assert_eq!(card.ai_review_stage.as_deref(), Some("reviewers"));
    assert_eq!(card.ai_review_badge.as_ref().unwrap().label, "AI reviewing…");
    for execution in &executions {
        let member = db.review_batch_member_for_execution(&execution.id).unwrap().unwrap();
        assert_eq!(member.role.stage(), ReviewStage::Reviewers);
    }

    // Both reviewers report and finish; the supervisor is queued, then runs.
    for (index, (execution, run)) in executions.iter().zip(&runs).enumerate() {
        submit_review_report(
            &db,
            &execution.id,
            &chore.id,
            &batch.id,
            "head-sha",
            &format!("report-{index}"),
        );
        db.finish_execution_run(
            FinishExecutionRunInput::builder()
                .execution_id(&execution.id)
                .run_id(&run.id)
                .execution_status(ExecutionStatus::Completed)
                .run_status("completed")
                .clear_workspace_lease(true)
                .build(),
        )
        .unwrap();
    }
    let supervisor = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .unwrap();
    let supervisor_execution = supervisor.execution_id.clone().unwrap();
    start_review_run(&db, &supervisor_execution, 9);

    let card = review_card(&db, &product.id, &chore.id);
    assert!(card.ai_reviewing);
    assert_eq!(card.ai_review_state.as_deref(), Some("reviewing"));
    assert_eq!(card.ai_review_stage.as_deref(), Some("consolidation"));
    assert_eq!(
        card.ai_review_badge.as_ref().unwrap().label,
        "Consolidating review findings"
    );
    let execution = db.get_execution(&supervisor_execution).unwrap();
    let metadata = crate::live_worker_metadata::resolve(&db, &execution, "local");
    assert_eq!(metadata.review_stage.as_deref(), Some("consolidation"));
    let reviewer = db.get_execution(&executions[0].id).unwrap();
    let metadata = crate::live_worker_metadata::resolve(&db, &reviewer, "local");
    assert_eq!(metadata.review_stage.as_deref(), Some("reviewers"));
}
