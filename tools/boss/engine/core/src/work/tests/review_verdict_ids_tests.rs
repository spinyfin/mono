//! Leaf finding ids and supervisor dispositions at the proposal submission
//! boundary: every cited leaf id must exist, and every leaf finding must be
//! either cited or listed in `rejected` — an omission is rejected, never
//! treated as an implicit rejection.
use super::*;

const PR_URL: &str = "https://github.com/example/repo/pull/42";
static NEXT_KEY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn report_finding(title: &str) -> serde_json::Value {
    serde_json::json!({
        "severity": "high", "category": "correctness", "confidence": "high",
        "file": "src/lib.rs", "title": title, "problem": "p", "impact": "i",
        "suggested_fix": "f", "static_evidence": "e", "needs_runtime_verification": false
    })
}

/// Submit a leaf report carrying `titles.len()` findings. A model-supplied
/// `id` on the first finding must be ignored in favour of the engine's.
fn submit_report_with_findings(
    db: &WorkDb,
    execution_id: &str,
    root_id: &str,
    batch_id: &str,
    titles: &[&str],
) -> SubmitWorkerProposalOutcome {
    let mut findings: Vec<_> = titles.iter().map(|title| report_finding(title)).collect();
    if let Some(first) = findings.first_mut() {
        first["id"] = serde_json::json!("model-picked");
    }
    let payload = serde_json::json!({
        "batch_id": batch_id, "target_sha": "head-sha",
        "report": {
            "batch_id": batch_id, "pr_url": PR_URL, "target_sha": "head-sha", "phase": "pre_merge",
            "summary": "Reported.",
            "coverage": {"files_inspected": [], "files_omitted": [], "limitations": []},
            "findings": findings
        }
    });
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id,
        work_item_id: root_id,
        kind: ProposalKind::ReviewReport,
        payload_json: &payload.to_string(),
        idempotency_key: "report",
    })
    .unwrap()
    .unwrap()
}

fn consolidated(id: &str, sources: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": id, "severity": "high", "category": "correctness", "confidence": "high",
        "file": "src/lib.rs", "title": "Consolidated", "detail": "Merged.", "sources": sources
    })
}

/// A supervising batch whose claude leaf reported two findings (`claude-1`,
/// `claude-2`) and whose codex leaf reported one (`codex-1`).
fn supervising_batch(db: &WorkDb) -> (String, String, String) {
    let product = create_test_product(db);
    let root = create_test_chore_manual(db, product.id, "review target");
    let ReviewBatchDispatch::Created { batch, executions } = db
        .create_pre_merge_review_batch(
            batch_input(root.id.clone(), "head-sha", ReviewBatchPhase::PreMerge),
            "https://github.com/example/repo",
        )
        .unwrap()
    else {
        panic!("expected new batch")
    };
    let titles: [&[&str]; 2] = [&["first claude defect", "second claude defect"], &["codex defect"]];
    for (execution, titles) in executions.iter().zip(titles) {
        assert_eq!(
            submit_report_with_findings(db, &execution.id, &root.id, &batch.id, titles)
                .proposal
                .state,
            ProposalState::Applied
        );
    }
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising
    );
    let supervisor = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|member| member.role == ReviewBatchMemberRole::Supervisor)
        .unwrap();
    (root.id, batch.id, supervisor.execution_id.unwrap())
}

fn submit_verdict(
    db: &WorkDb,
    root_id: &str,
    batch_id: &str,
    supervisor_execution_id: &str,
    findings: serde_json::Value,
    rejected: serde_json::Value,
) -> boss_protocol::WorkerProposal {
    let payload = serde_json::json!({
        "batch_id": batch_id,
        "verdict": {
            "batch_id": batch_id, "pr_url": PR_URL, "target_sha": "head-sha", "phase": "pre_merge",
            "summary": "Consolidated.", "revision_warranted": true,
            "findings": findings, "rejected": rejected, "contradictions": []
        }
    });
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id: supervisor_execution_id,
        work_item_id: root_id,
        kind: ProposalKind::ReviewVerdict,
        payload_json: &payload.to_string(),
        idempotency_key: &format!("verdict-{}", NEXT_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)),
    })
    .unwrap()
    .unwrap()
    .proposal
}

fn rejection_reason(proposal: &boss_protocol::WorkerProposal) -> String {
    assert_eq!(proposal.state, ProposalState::Rejected, "{proposal:?}");
    proposal.decision_reason.clone().unwrap_or_default()
}

#[test]
fn a_verdict_accounting_for_every_leaf_finding_is_accepted() {
    let db = WorkDb::open(temp_db_path("verdict-ids-accounted")).unwrap();
    let (root, batch, supervisor) = supervising_batch(&db);
    let proposal = submit_verdict(
        &db,
        &root,
        &batch,
        &supervisor,
        serde_json::json!([consolidated("C1", &["claude-1", "codex-1"])]),
        serde_json::json!([{"leaf_finding_id": "claude-2", "reason": "Re-read the file; not a defect."}]),
    );
    assert_eq!(proposal.state, ProposalState::Proposed, "{proposal:?}");
}

#[test]
fn a_verdict_citing_an_unknown_leaf_finding_id_is_rejected_loudly() {
    let db = WorkDb::open(temp_db_path("verdict-ids-unknown")).unwrap();
    let (root, batch, supervisor) = supervising_batch(&db);
    // `claude-3` does not exist, and the model-picked id on claude's first
    // finding was never adopted.
    for unknown in ["claude-3", "model-picked"] {
        let proposal = submit_verdict(
            &db,
            &root,
            &batch,
            &supervisor,
            serde_json::json!([consolidated("C1", &[unknown, "claude-1", "claude-2", "codex-1"])]),
            serde_json::json!([]),
        );
        let reason = rejection_reason(&proposal);
        assert!(
            reason.contains(&format!("unknown leaf finding id `{unknown}`")),
            "{reason}"
        );
    }
}

#[test]
fn a_verdict_rejecting_an_unknown_leaf_finding_id_is_rejected_loudly() {
    let db = WorkDb::open(temp_db_path("verdict-ids-unknown-rejected")).unwrap();
    let (root, batch, supervisor) = supervising_batch(&db);
    let proposal = submit_verdict(
        &db,
        &root,
        &batch,
        &supervisor,
        serde_json::json!([consolidated("C1", &["claude-1", "claude-2", "codex-1"])]),
        serde_json::json!([{"leaf_finding_id": "codex-9", "reason": "made up"}]),
    );
    assert!(rejection_reason(&proposal).contains("unknown leaf finding id `codex-9`"));
}

#[test]
fn a_verdict_leaving_a_leaf_finding_unaccounted_is_rejected_not_auto_rejected() {
    let db = WorkDb::open(temp_db_path("verdict-ids-unaccounted")).unwrap();
    let (root, batch, supervisor) = supervising_batch(&db);
    // `claude-2` is neither merged into a finding nor listed in `rejected`.
    let proposal = submit_verdict(
        &db,
        &root,
        &batch,
        &supervisor,
        serde_json::json!([consolidated("C1", &["claude-1", "codex-1"])]),
        serde_json::json!([]),
    );
    let reason = rejection_reason(&proposal);
    assert!(reason.contains("`claude-2`") && reason.contains("neither cited"), "{reason}");
    assert!(!reason.contains("`claude-1`") && !reason.contains("`codex-1`"), "{reason}");
    // Nothing was recorded as rejected on the supervisor's behalf, and the
    // batch is still waiting for a corrected verdict.
    assert_eq!(
        db.review_batch(&batch).unwrap().unwrap().status,
        ReviewBatchStatus::Supervising
    );
}

#[test]
fn a_leaf_finding_cannot_be_both_cited_and_rejected() {
    let db = WorkDb::open(temp_db_path("verdict-ids-both")).unwrap();
    let (root, batch, supervisor) = supervising_batch(&db);
    let proposal = submit_verdict(
        &db,
        &root,
        &batch,
        &supervisor,
        serde_json::json!([consolidated("C1", &["claude-1", "claude-2", "codex-1"])]),
        serde_json::json!([{"leaf_finding_id": "claude-2", "reason": "also dropped?"}]),
    );
    assert!(rejection_reason(&proposal).contains("both cited"));
}
