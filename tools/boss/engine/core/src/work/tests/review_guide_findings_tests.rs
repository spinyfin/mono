use super::*;

fn apply_findings(db: &WorkDb, root: &str, sha: &str) -> String {
    let supervisor = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let reviewer = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(root)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Completed)
                .build(),
        )
        .unwrap();
    let (batch, _) = db
        .create_review_batch(
            batch_input(root.to_owned(), sha),
            &[
                member(
                    ReviewBatchMemberRole::ClaudeReviewer,
                    Some(reviewer.id),
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
            work_item_id: root,
            kind: ProposalKind::ReviewVerdict,
            payload_json: &findings_verdict_payload(&batch.id, sha),
            idempotency_key: sha,
        })
        .unwrap()
        .unwrap();
    db.apply_review_verdict_proposal(&outcome.proposal.id, &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap()
        .unwrap()
}

fn findings(db: &WorkDb, root: &str) -> Option<boss_protocol::ReviewGuideFindings> {
    let WorkItem::Chore(task) = db.get_work_item(root).unwrap() else {
        panic!("expected chore")
    };
    db.review_guide_findings(root, task.pr_url.as_deref().unwrap()).unwrap()
}

#[test]
fn live_addendum_tracks_findings_and_status_without_mutating_guide() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "review target");
    bind_open_pr(&db, &root.id);
    let capture = db
        .persist_pr_review_guide_source_capture(
            &root.id,
            1,
            PrSourceCaptureTrigger::Creation,
            &crate::test_support::source_capture_packet(PR_URL, "base", "head"),
        )
        .unwrap();
    let PrSourceCapturePersistOutcome::Stored(capture) = capture else {
        panic!("capture")
    };
    let attempt = db
        .create_pr_review_guide_attempt(&capture.series_id, &capture.comparison_id, "test")
        .unwrap();
    let PublishReviewGuideOutcome::Published(version) = db
        .publish_pr_review_guide_version(&attempt.id, "# Original guide", "raw")
        .unwrap()
    else {
        panic!("publish")
    };
    assert!(findings(&db, &root.id).is_none());
    let revision = apply_findings(&db, &root.id, "head-one");
    let before = findings(&db, &root.id).unwrap();
    let tracked = query_task(&db.connect().unwrap(), &revision).unwrap().unwrap();
    let label = boss_protocol::short_id_label(tracked.short_id).unwrap();
    assert!(before.status_text.starts_with("AI review found 1 issue;"));
    assert_eq!(
        before.addendum_markdown,
        format!("- [high] Unchecked index — ID {label} (todo)")
    );
    assert!(!before.status_text.contains("fixes complete"));

    for status in ["active", "blocked", "in_review", "archived", "done"] {
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![revision, status])
            .unwrap();
        let updated = findings(&db, &root.id).unwrap();
        assert!(updated.addendum_markdown.contains(&format!("ID {label} ({status})")));
        assert_eq!(updated.status_text.contains("fixes complete"), status == "done");
    }
    let stored = db.get_pr_review_guide_version(&version.id).unwrap().unwrap();
    assert_eq!(stored.markdown, version.markdown);
    assert_eq!(stored.content_hash, version.content_hash);
    apply_findings(&db, &root.id, "head-two");
    let updated = findings(&db, &root.id).unwrap();
    assert!(updated.status_text.starts_with("AI review found 2 issues;"));
    assert_eq!(
        updated
            .addendum_markdown
            .lines()
            .filter(|line| line.starts_with("- "))
            .count(),
        2
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET pr_url = 'https://github.com/example/repo/pull/43' WHERE id = ?1",
            [&root.id],
        )
        .unwrap();
    assert!(
        findings(&db, &root.id).is_none(),
        "a replacement PR must not inherit the old addendum"
    );
}

#[test]
fn merge_gate_reports_all_open_revisions_and_ignores_terminal_or_deleted_items() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "merge target");
    bind_open_pr(&db, &root.id);
    assert!(db.open_merge_revisions(&root.id).unwrap().is_empty());
    let revision = apply_findings(&db, &root.id, "head-one");
    for status in ["todo", "active", "blocked", "in_review", "done", "archived"] {
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![revision, status])
            .unwrap();
        let open = db.open_merge_revisions(&root.id).unwrap();
        if ["done", "archived"].contains(&status) {
            assert!(open.is_empty());
        } else {
            assert_eq!(open.len(), 1);
            assert_eq!(open[0].id, revision);
            assert_eq!(open[0].status, status);
        }
    }
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', deleted_at = 'now' WHERE id = ?1",
            [&revision],
        )
        .unwrap();
    assert!(db.open_merge_revisions(&root.id).unwrap().is_empty());
    let child = db
        .create_revision(revision_input(&root.id), &FakePrStateChecker::always(PrOpenState::Open))
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET parent_task_id = ?2 WHERE id = ?1",
            params![child.id, revision],
        )
        .unwrap();
    let open = db.open_merge_revisions(&root.id).unwrap();
    assert_eq!(open.len(), 1, "live nested revisions survive a deleted intermediary");
    assert_eq!(open[0].id, child.id);
}

#[test]
fn addendum_includes_followup_tracking_after_the_origin_merged() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "merged target");
    bind_merged_pr(&db, &root.id);
    let tracker = apply_findings(&db, &root.id, "head-one");
    let item = query_task(&db.connect().unwrap(), &tracker).unwrap().unwrap();
    assert_eq!(item.kind, TaskKind::Followup);
    assert!(
        findings(&db, &root.id)
            .unwrap()
            .addendum_markdown
            .contains("Unchecked index")
    );
    assert!(
        db.open_merge_revisions(&root.id).unwrap().is_empty(),
        "followups do not block the old PR"
    );
}

#[test]
fn legacy_verdict_uses_engine_rendered_revision_titles() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "legacy target");
    bind_open_pr(&db, &root.id);
    let revision = apply_findings(&db, &root.id, "head-one");
    // Model a pre-batch verdict: the full finding is retained in the
    // generated revision brief and the reviewed PR in its execution.
    let conn = db.connect().unwrap();
    conn.execute("UPDATE work_executions SET pr_url = ?1 WHERE id IN (SELECT execution_id FROM pr_review_verdicts WHERE revision_task_id = ?2)", params![PR_URL, revision]).unwrap();
    conn.execute(
        "UPDATE pr_review_verdicts SET proposal_id = NULL, batch_id = NULL WHERE revision_task_id = ?1",
        [&revision],
    )
    .unwrap();
    drop(conn);
    let text = findings(&db, &root.id).unwrap();
    assert!(text.addendum_markdown.contains("[high] Unchecked index"));
}
