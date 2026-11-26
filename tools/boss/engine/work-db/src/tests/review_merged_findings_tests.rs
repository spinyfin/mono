use super::*;

fn reported_batch(db: &WorkDb) -> (Task, ReviewBatch, String) {
    reported_batch_with_severity(db, "high")
}

fn reported_batch_with_severity(db: &WorkDb, severity: &str) -> (Task, ReviewBatch, String) {
    let product = create_test_product(db);
    let root = create_test_chore_manual(db, product.id, "review target");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/example/repo/pull/42' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    let (batch, supervisor) = add_reported_batch(db, &root, "reviewed-head", severity);
    (root, batch, supervisor)
}

fn add_reported_batch(db: &WorkDb, root: &Task, target_sha: &str, severity: &str) -> (ReviewBatch, String) {
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
        .map(|(role, exec)| member(*role, Some(exec.id.clone())))
        .collect();
    let (batch, _) = db
        .create_review_batch(
            batch_input(root.id.clone(), target_sha, ReviewBatchPhase::PreMerge),
            &members,
        )
        .unwrap();
    for (role, exec) in roles.iter().zip(&executions) {
        let payload = serde_json::json!({
            "batch_id": batch.id, "target_sha": batch.target_sha,
            "report": {"batch_id": batch.id, "pr_url": batch.pr_url, "target_sha": batch.target_sha,
                "phase": "pre_merge", "summary": format!("Report from {role}"),
                "coverage": {"files_inspected": ["src/lib.rs"], "files_omitted": [], "limitations": ["static only"]},
                "findings": [{"severity": severity, "category": "correctness", "confidence": "high",
                    "file": "src/lib.rs", "title": format!("Finding from {role} at {target_sha}"), "problem": "lost result",
                    "impact": "lost work", "suggested_fix": "persist it", "static_evidence": "early return",
                    "needs_runtime_verification": true}]
            }
        })
        .to_string();
        db.submit_worker_proposal(SubmitWorkerProposalInput {
            execution_id: &exec.id,
            work_item_id: &root.id,
            kind: ProposalKind::ReviewReport,
            payload_json: &payload,
            idempotency_key: "report",
        })
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
    (batch, supervisor)
}

fn merged_findings_disposition(db: &WorkDb, batch_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT merged_findings_disposition FROM pr_review_batches WHERE id = ?1",
            [batch_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn unconsolidated_followup(db: &WorkDb, batch_id: &str) -> Option<crate::Task> {
    let conn = db.connect().unwrap();
    crate::review_findings_followup::existing_review_findings_work_item(
        &conn,
        &format!("pr_review:unconsolidated:{batch_id}"),
    )
    .unwrap()
}

fn stamp_batch_created_at(db: &WorkDb, batch_id: &str, created_at: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET created_at = ?2 WHERE id = ?1",
            rusqlite::params![batch_id, created_at],
        )
        .unwrap();
}

#[test]
fn low_severity_reports_remain_readable_without_forcing_remediation() {
    let db = WorkDb::open(temp_db_path("merged-review-low-severity")).unwrap();
    let (root, batch, supervisor) = reported_batch_with_severity(&db, "low");
    db.fail_review_batch_for_closed_pr(&supervisor).unwrap();
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    assert_eq!(
        merged_findings_disposition(&db, &batch.id).as_deref(),
        Some("below_gate")
    );
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    let reports = db.review_batch_reports(&batch.id).unwrap();
    assert_eq!(reports.len(), 3);
    assert!(
        reports
            .iter()
            .all(|read| read.report.as_ref().unwrap().findings.len() == 1)
    );
}

#[test]
fn a_replacement_batch_without_reports_does_not_drop_the_original_findings() {
    let db = WorkDb::open(temp_db_path("merged-review-superseded")).unwrap();
    let (root, batch, supervisor) = reported_batch(&db);
    db.fail_review_batch_for_moved_head(&supervisor, "new-head").unwrap();
    db.create_review_batch(
        batch_input(root.id.clone(), "new-head", ReviewBatchPhase::PreMerge),
        &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
    )
    .unwrap();
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    db.reap_inert_review_batches(REVIEW_BATCH_STALE_SECS).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    assert_eq!(db.review_batch_reports(&batch.id).unwrap().len(), 3);
    let followup = unconsolidated_followup(&db, &batch.id).unwrap();
    assert!(followup.description.contains("Finding from claude_reviewer"));
    assert_eq!(merged_findings_disposition(&db, &batch.id).as_deref(), Some("followup"));
}

#[test]
fn a_newer_completed_review_skips_the_superseded_batch_findings() {
    let db = WorkDb::open(temp_db_path("merged-review-newer-completed")).unwrap();
    let (root, batch, supervisor) = reported_batch(&db);
    db.fail_review_batch_for_moved_head(&supervisor, "new-head").unwrap();
    let (newer, _) = db
        .create_review_batch(
            batch_input(root.id.clone(), "new-head", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches SET status = 'completed' WHERE id = ?1",
            [&newer.id],
        )
        .unwrap();
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    assert_eq!(db.review_batch_reports(&batch.id).unwrap().len(), 3);
}

#[test]
fn staged_verdict_keeps_its_apply_owner_after_merge() {
    let db = WorkDb::open(temp_db_path("merged-review-staged-verdict")).unwrap();
    let (root, batch, supervisor) = reported_batch(&db);
    let rejected: Vec<_> = db
        .review_batch_reports(&batch.id)
        .unwrap()
        .into_iter()
        .flat_map(|read| read.report.unwrap().findings)
        .map(|f| {
            serde_json::json!({
                "leaf_finding_id": f.id.unwrap(), "reason": "Verified existing persistence"
            })
        })
        .collect();
    let payload = serde_json::json!({"batch_id": batch.id, "verdict": {
        "batch_id": batch.id, "pr_url": batch.pr_url, "target_sha": batch.target_sha,
        "phase": "pre_merge", "summary": "Verified reports", "revision_warranted": false,
        "findings": [], "rejected": rejected
    }})
    .to_string();
    db.submit_worker_proposal(SubmitWorkerProposalInput {
        execution_id: &supervisor,
        work_item_id: &root.id,
        kind: ProposalKind::ReviewVerdict,
        payload_json: &payload,
        idempotency_key: "verdict",
    })
    .unwrap()
    .unwrap();
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Applying
    );
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    assert!(db.reap_inert_review_batches(0).unwrap().is_empty());
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Applying
    );
}

#[test]
fn merged_before_supervisor_start_preserves_all_reports_once() {
    let db = WorkDb::open(temp_db_path("merged-review-findings")).unwrap();
    let (root, batch, supervisor) = reported_batch(&db);
    assert!(db.get_execution(&supervisor).unwrap().started_at.is_none());
    db.connect()
        .unwrap()
        .execute(
            "UPDATE work_executions SET status = 'failed' WHERE id = ?1",
            [&supervisor],
        )
        .unwrap();
    assert!(db.fail_review_batch_for_closed_pr(&supervisor).unwrap());
    assert_eq!(
        db.preserve_merged_review_batch_findings().unwrap(),
        0,
        "failure is not merge evidence"
    );
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    let conn = db.connect().unwrap();
    let followup = crate::review_findings_followup::existing_review_findings_work_item(
        &conn,
        &format!("pr_review:unconsolidated:{}", batch.id),
    )
    .unwrap()
    .unwrap();
    drop(conn);
    assert_eq!(followup.kind, TaskKind::Followup);
    assert_eq!(followup.origin_pr_number, Some(42));
    assert!(followup.parent_task_id.is_none());
    assert!(followup.pr_url.is_none());
    assert!(followup.description.contains("unconsolidated"));
    for role in ["claude_reviewer", "codex_reviewer", "grok_reviewer"] {
        assert!(followup.description.contains(&format!("Finding from {role}")));
    }
    for evidence in [
        "reviewed-head",
        "static only",
        "early return",
        "Needs runtime verification: true",
        "persist it",
    ] {
        assert!(followup.description.contains(evidence), "missing {evidence}");
    }
    assert_eq!(db.review_batch_reports(&batch.id).unwrap().len(), 3);
    assert!(db.review_verdicts_for_work_item(&root.id).unwrap().is_empty());
    assert_eq!(
        db.review_batch(&batch.id).unwrap().unwrap().status,
        ReviewBatchStatus::Failed
    );
    let quorum = db
        .list_attention_items_for_work_item(&root.id)
        .unwrap()
        .into_iter()
        .find(|a| a.kind == "pr_review_quorum_failed" && a.status == "open")
        .unwrap();
    assert!(quorum.body_markdown.contains(&followup.id), "{}", quorum.body_markdown);
    assert!(
        quorum.body_markdown.contains("unconsolidated"),
        "{}",
        quorum.body_markdown
    );
    assert_eq!(db.get_task_review_cycle_state(&root.id).unwrap().0, 0);
}

#[test]
fn merge_observed_before_recovery_reaps_then_preserves_reports() {
    let db = WorkDb::open(temp_db_path("merged-review-reaped")).unwrap();
    let (root, batch, _) = reported_batch(&db);
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    assert_eq!(
        db.reap_inert_review_batches(REVIEW_BATCH_STALE_SECS).unwrap(),
        vec![batch.id.clone()]
    );
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    assert_eq!(db.review_batch_reports(&batch.id).unwrap().len(), 3);
    let followup = unconsolidated_followup(&db, &batch.id).unwrap();
    let stale = db
        .list_attention_items_for_work_item(&root.id)
        .unwrap()
        .into_iter()
        .find(|a| a.kind == crate::PR_REVIEW_BATCH_STALE_ATTENTION_KIND && a.status == "open")
        .unwrap();
    assert!(stale.body_markdown.contains(&followup.id), "{}", stale.body_markdown);
}

#[test]
fn manual_done_or_rebound_pr_does_not_invent_merge_evidence() {
    let db = WorkDb::open(temp_db_path("merged-review-binding")).unwrap();
    let (root, batch, supervisor) = reported_batch(&db);
    db.fail_review_batch_for_closed_pr(&supervisor).unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&root.id])
        .unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/example/repo/pull/43' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
}

#[test]
fn a_corrupt_accepted_report_does_not_drop_the_readable_members() {
    let db = WorkDb::open(temp_db_path("merged-review-corrupt-report")).unwrap();
    let (root, batch, supervisor) = reported_batch(&db);
    let first_proposal = db
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
            [&first_proposal],
        )
        .unwrap();
    db.fail_review_batch_for_closed_pr(&supervisor).unwrap();
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    let reads = db.review_batch_reports(&batch.id).unwrap();
    assert_eq!(reads.len(), 3);
    assert_eq!(reads.iter().filter(|read| read.report.is_ok()).count(), 2);
    assert!(
        reads
            .iter()
            .any(|read| read.member.role == ReviewBatchMemberRole::ClaudeReviewer && read.report.is_err())
    );
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    let followup = unconsolidated_followup(&db, &batch.id).unwrap();
    assert!(followup.description.contains("Finding from codex_reviewer"));
    assert!(followup.description.contains("Finding from grok_reviewer"));
    assert!(followup.description.contains("Unreadable accepted report"));
    let quorum = db
        .list_attention_items_for_work_item(&root.id)
        .unwrap()
        .into_iter()
        .find(|a| a.kind == "pr_review_quorum_failed" && a.status == "open")
        .unwrap();
    assert!(quorum.body_markdown.contains(&followup.id), "{}", quorum.body_markdown);
    assert!(
        quorum.body_markdown.contains("could not be read"),
        "{}",
        quorum.body_markdown
    );
}

#[test]
fn a_newer_failed_staged_verdict_does_not_drop_the_older_findings() {
    let db = WorkDb::open(temp_db_path("merged-review-newer-staged-failed")).unwrap();
    let (root, older, supervisor) = reported_batch(&db);
    db.fail_review_batch_for_moved_head(&supervisor, "new-head").unwrap();
    let (newer, _) = db
        .create_review_batch(
            batch_input(root.id.clone(), "new-head", ReviewBatchPhase::PreMerge),
            &[member(ReviewBatchMemberRole::ClaudeReviewer, None)],
        )
        .unwrap();
    stamp_batch_created_at(&db, &older.id, "1");
    stamp_batch_created_at(&db, &newer.id, "2");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_batches
             SET status = 'failed', final_verdict_proposal_id = 'staged-verdict'
             WHERE id = ?1",
            [&newer.id],
        )
        .unwrap();
    db.observe_chore_pr_merged(&root.id, &older.pr_url).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    assert!(unconsolidated_followup(&db, &older.id).is_some());
    assert!(unconsolidated_followup(&db, &newer.id).is_none());
}

#[test]
fn two_failed_reported_batches_each_get_a_followup() {
    let db = WorkDb::open(temp_db_path("merged-review-two-failed")).unwrap();
    let (root, older, older_supervisor) = reported_batch(&db);
    db.fail_review_batch_for_closed_pr(&older_supervisor).unwrap();
    let (newer, newer_supervisor) = add_reported_batch(&db, &root, "new-head", "high");
    db.fail_review_batch_for_closed_pr(&newer_supervisor).unwrap();
    stamp_batch_created_at(&db, &older.id, "1");
    stamp_batch_created_at(&db, &newer.id, "2");
    db.observe_chore_pr_merged(&root.id, &older.pr_url).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 2);
    let older_followup = unconsolidated_followup(&db, &older.id).unwrap();
    let newer_followup = unconsolidated_followup(&db, &newer.id).unwrap();
    assert!(
        older_followup
            .description
            .contains("Finding from claude_reviewer at reviewed-head")
    );
    assert!(
        newer_followup
            .description
            .contains("Finding from claude_reviewer at new-head")
    );
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
}

#[test]
fn older_batch_is_retried_after_a_newer_batch_preserves_first() {
    let db = WorkDb::open(temp_db_path("merged-review-older-retry")).unwrap();
    let (root, older, older_supervisor) = reported_batch(&db);
    db.fail_review_batch_for_closed_pr(&older_supervisor).unwrap();
    let (newer, newer_supervisor) = add_reported_batch(&db, &root, "new-head", "high");
    db.fail_review_batch_for_closed_pr(&newer_supervisor).unwrap();
    stamp_batch_created_at(&db, &older.id, "1");
    stamp_batch_created_at(&db, &newer.id, "2");
    db.observe_chore_pr_merged(&root.id, &older.pr_url).unwrap();
    db.connect()
        .unwrap()
        .execute_batch(&format!(
            "CREATE TRIGGER fail_older_preserve BEFORE INSERT ON tasks
             WHEN NEW.created_via = 'pr_review:unconsolidated:{}'
             BEGIN
               SELECT RAISE(ABORT, 'injected preservation error');
             END;",
            older.id
        ))
        .unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    assert!(unconsolidated_followup(&db, &older.id).is_none());
    assert!(unconsolidated_followup(&db, &newer.id).is_some());
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER fail_older_preserve")
        .unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 1);
    assert!(unconsolidated_followup(&db, &older.id).is_some());
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
}

#[test]
fn gated_unreadable_batch_is_not_reselected_on_the_next_sweep() {
    let db = WorkDb::open(temp_db_path("merged-review-gated-unreadable")).unwrap();
    let (root, batch, supervisor) = reported_batch_with_severity(&db, "low");
    let first_proposal = db
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
            [&first_proposal],
        )
        .unwrap();
    db.fail_review_batch_for_closed_pr(&supervisor).unwrap();
    db.observe_chore_pr_merged(&root.id, &batch.pr_url).unwrap();
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    assert_eq!(
        merged_findings_disposition(&db, &batch.id).as_deref(),
        Some("unreadable")
    );
    assert!(unconsolidated_followup(&db, &batch.id).is_none());
    let quorum = db
        .list_attention_items_for_work_item(&root.id)
        .unwrap()
        .into_iter()
        .find(|a| a.kind == "pr_review_quorum_failed" && a.status == "open")
        .unwrap();
    assert!(
        quorum.body_markdown.contains("could not be read"),
        "{}",
        quorum.body_markdown
    );
    assert_eq!(db.preserve_merged_review_batch_findings().unwrap(), 0);
    assert_eq!(
        merged_findings_disposition(&db, &batch.id).as_deref(),
        Some("unreadable")
    );
}
