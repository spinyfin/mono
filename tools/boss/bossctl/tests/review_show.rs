//! Exercise the actual read-only CLI against a failed batch with accepted reports.
use boss_engine::work::{ReviewBatchCreateInput, ReviewBatchMemberCreateInput, SubmitWorkerProposalInput, WorkDb};
use boss_protocol::*;

#[test]
fn review_show_exposes_completed_reports_and_failed_quorum_without_a_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Review fixture")
                .repo_remote_url("https://github.com/example/repo")
                .build(),
        )
        .unwrap();
    let root = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name("Unconsolidated review")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let roles = [
        ReviewBatchMemberRole::ClaudeReviewer,
        ReviewBatchMemberRole::CodexReviewer,
        ReviewBatchMemberRole::GrokReviewer,
    ];
    let executions: Vec<_> = roles
        .iter()
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&root.id)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap()
        })
        .collect();
    let members: Vec<_> = roles
        .iter()
        .zip(&executions)
        .map(|(role, exec)| {
            ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("fixture")
                .role(*role)
                .status(ReviewBatchMemberStatus::Pending)
                .execution_id(&exec.id)
                .build()
        })
        .collect();
    let (batch, _) = db
        .create_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(&root.id)
                .base_sha("base")
                .target_sha("head")
                .pr_number(42)
                .pr_url("https://github.com/example/repo/pull/42")
                .phase(ReviewBatchPhase::PreMerge)
                .classification(
                    ReviewClassification::builder()
                        .changed_files(vec![])
                        .complexity_flags(vec![])
                        .has_production_code(true)
                        .metadata_missing(vec![])
                        .production_languages(vec![])
                        .profile(ReviewProfile::Light)
                        .subsystem_buckets(vec![])
                        .additions(1)
                        .deletions(0)
                        .build(),
                )
                .build(),
            &members,
        )
        .unwrap();
    for exec in &executions {
        let payload = serde_json::json!({"batch_id": batch.id, "target_sha": "head", "report": {
            "batch_id": batch.id, "pr_url": batch.pr_url, "target_sha": "head", "phase": "pre_merge",
            "summary": "Completed static review", "coverage": {"files_inspected": ["src/lib.rs"], "files_omitted": [], "limitations": []},
            "findings": [{"severity": "high", "category": "correctness", "confidence": "high",
                "file": "src/lib.rs", "title": "Lost review output", "problem": "early return drops reports",
                "impact": "findings disappear", "suggested_fix": "preserve reports", "static_evidence": "return before save",
                "needs_runtime_verification": false}]
        }}).to_string();
        db.submit_worker_proposal_with_liveness(
            SubmitWorkerProposalInput {
                execution_id: &exec.id,
                work_item_id: &root.id,
                kind: ProposalKind::ReviewReport,
                payload_json: &payload,
                idempotency_key: "report",
            },
            &|_| false,
        )
        .unwrap()
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
                [&exec.id],
            )
            .unwrap();
    }
    let supervisor = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|m| m.role == ReviewBatchMemberRole::Supervisor)
        .unwrap()
        .execution_id
        .unwrap();
    assert!(db.fail_review_batch_for_closed_pr(&supervisor).unwrap());
    drop(db);
    let show = |json: bool| {
        let mut command = std::process::Command::new(std::env::var("BOSSCTL_TEST_BINARY").unwrap());
        if json {
            command.arg("--json");
        }
        let output = command
            .args(["review", "show", &root.id, "--state-root", dir.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };
    let text = show(false);
    assert!(!text.contains("never reviewed"), "{text}");
    assert!(text.contains("3 completed member report(s)"), "{text}");
    assert!(text.contains("failed quorum"), "{text}");
    assert!(text.contains("Lost review output"), "{text}");
    assert!(text.contains("return before save"), "{text}");
    let json: serde_json::Value = serde_json::from_str(&show(true)).unwrap();
    assert_eq!(json["batches"][0]["batch"]["status"], "failed");
    assert_eq!(json["batches"][0]["reports"].as_array().unwrap().len(), 3);
    assert_eq!(
        json["batches"][0]["reports"][0]["report"]["findings"][0]["title"],
        "Lost review output"
    );
    assert!(json["latest_informative_verdict"].is_null());
}

#[test]
fn review_show_keeps_readable_reports_when_one_payload_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("state.db")).unwrap();
    let product = db
        .create_product(
            CreateProductInput::builder()
                .name("Review fixture")
                .repo_remote_url("https://github.com/example/repo")
                .build(),
        )
        .unwrap();
    let root = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id)
                .name("Unconsolidated review")
                .autostart(false)
                .build(),
        )
        .unwrap();
    let roles = [
        ReviewBatchMemberRole::ClaudeReviewer,
        ReviewBatchMemberRole::CodexReviewer,
        ReviewBatchMemberRole::GrokReviewer,
    ];
    let executions: Vec<_> = roles
        .iter()
        .map(|_| {
            db.create_execution(
                CreateExecutionInput::builder()
                    .work_item_id(&root.id)
                    .kind(ExecutionKind::PrReview)
                    .status(ExecutionStatus::Running)
                    .build(),
            )
            .unwrap()
        })
        .collect();
    let members: Vec<_> = roles
        .iter()
        .zip(&executions)
        .map(|(role, exec)| {
            ReviewBatchMemberCreateInput::builder()
                .attempt(1)
                .provider_effort("medium")
                .requested_driver("claude")
                .resolved_model("fixture")
                .role(*role)
                .status(ReviewBatchMemberStatus::Pending)
                .execution_id(&exec.id)
                .build()
        })
        .collect();
    let (batch, _) = db
        .create_review_batch(
            ReviewBatchCreateInput::builder()
                .cycle_root_id(&root.id)
                .base_sha("base")
                .target_sha("head")
                .pr_number(42)
                .pr_url("https://github.com/example/repo/pull/42")
                .phase(ReviewBatchPhase::PreMerge)
                .classification(
                    ReviewClassification::builder()
                        .changed_files(vec![])
                        .complexity_flags(vec![])
                        .has_production_code(true)
                        .metadata_missing(vec![])
                        .production_languages(vec![])
                        .profile(ReviewProfile::Light)
                        .subsystem_buckets(vec![])
                        .additions(1)
                        .deletions(0)
                        .build(),
                )
                .build(),
            &members,
        )
        .unwrap();
    for exec in &executions {
        let payload = serde_json::json!({"batch_id": batch.id, "target_sha": "head", "report": {
            "batch_id": batch.id, "pr_url": batch.pr_url, "target_sha": "head", "phase": "pre_merge",
            "summary": "Completed static review", "coverage": {"files_inspected": ["src/lib.rs"], "files_omitted": [], "limitations": []},
            "findings": [{"severity": "high", "category": "correctness", "confidence": "high",
                "file": "src/lib.rs", "title": "Lost review output", "problem": "early return drops reports",
                "impact": "findings disappear", "suggested_fix": "preserve reports", "static_evidence": "return before save",
                "needs_runtime_verification": false}]
        }}).to_string();
        db.submit_worker_proposal_with_liveness(
            SubmitWorkerProposalInput {
                execution_id: &exec.id,
                work_item_id: &root.id,
                kind: ProposalKind::ReviewReport,
                payload_json: &payload,
                idempotency_key: "report",
            },
            &|_| false,
        )
        .unwrap()
        .unwrap();
        db.connect()
            .unwrap()
            .execute(
                "UPDATE work_executions SET status = 'completed', finished_at = '1' WHERE id = ?1",
                [&exec.id],
            )
            .unwrap();
    }
    let claude_proposal = db
        .review_batch_members(&batch.id)
        .unwrap()
        .into_iter()
        .find(|m| m.role == ReviewBatchMemberRole::ClaudeReviewer)
        .unwrap()
        .report_proposal_id
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE worker_proposals SET payload_json = '{not-json' WHERE id = ?1",
            [&claude_proposal],
        )
        .unwrap();
    drop(db);
    let mut command = std::process::Command::new(std::env::var("BOSSCTL_TEST_BINARY").unwrap());
    let output = command
        .args(["review", "show", &root.id, "--state-root", dir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("2 completed member report(s)"), "{text}");
    assert!(text.contains("report unreadable:"), "{text}");
    assert!(text.contains("Lost review output"), "{text}");
    let mut json_cmd = std::process::Command::new(std::env::var("BOSSCTL_TEST_BINARY").unwrap());
    let json_output = json_cmd
        .args([
            "--json",
            "review",
            "show",
            &root.id,
            "--state-root",
            dir.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        json_output.status.success(),
        "{}",
        String::from_utf8_lossy(&json_output.stderr)
    );
    let json: serde_json::Value = serde_json::from_str(&String::from_utf8(json_output.stdout).unwrap()).unwrap();
    let reports = json["batches"][0]["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 3);
    assert_eq!(reports.iter().filter(|r| r.get("error").is_some()).count(), 1);
    assert_eq!(reports.iter().filter(|r| r.get("report").is_some()).count(), 2);
}
