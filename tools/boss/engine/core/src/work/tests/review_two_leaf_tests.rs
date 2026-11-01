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
                    "severity": "high", "category": "correctness", "confidence": "high",
                    "file": "src/lib.rs", "title": "Unchecked index", "detail": "Missing bounds check",
                    "sources": ["grok"]
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
