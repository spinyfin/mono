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
    assert_eq!(before.status_text, "1 finding: 0 fixed on PR, 1 in progress, 0 open");
    assert_eq!(
        before.addendum_markdown,
        format!("- ◷ [high] Unchecked index — ID {label} — in progress")
    );

    for (status, marker, counts) in [
        ("todo", "◷ ", "0 fixed on PR, 1 in progress, 0 open"),
        ("active", "◷ ", "0 fixed on PR, 1 in progress, 0 open"),
        ("blocked", "◷ ", "0 fixed on PR, 1 in progress, 0 open"),
        ("in_review", "✓ ~~", "1 fixed on PR, 0 in progress, 0 open"),
        ("archived", "", "0 fixed on PR, 0 in progress, 1 open"),
        ("done", "✓ ~~", "1 fixed on PR, 0 in progress, 0 open"),
    ] {
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![revision, status])
            .unwrap();
        let updated = findings(&db, &root.id).unwrap();
        assert_eq!(updated.status_text, format!("1 finding: {counts}"), "{status}");
        assert!(
            updated
                .addendum_markdown
                .contains(&format!("- {marker}[high] Unchecked index")),
            "{status}"
        );
        assert!(updated.addendum_markdown.contains(&format!("ID {label}")));
        assert!(!updated.addendum_markdown.contains(&format!("({status})")));
        if matches!(status, "in_review" | "done") {
            assert!(
                updated
                    .addendum_markdown
                    .ends_with(&format!("- ✓ ~~[high] Unchecked index~~ — ID {label}"))
            );
        }
    }
    let stored = db.get_pr_review_guide_version(&version.id).unwrap().unwrap();
    assert_eq!(stored.markdown, version.markdown);
    assert_eq!(stored.content_hash, version.content_hash);
    apply_findings(&db, &root.id, "head-two");
    let updated = findings(&db, &root.id).unwrap();
    assert_eq!(updated.status_text, "2 findings: 1 fixed on PR, 1 in progress, 0 open");
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
        if ["todo", "active", "blocked"].contains(&status) {
            assert_eq!(open.len(), 1);
            assert_eq!(open[0].id, revision);
            assert_eq!(open[0].status, status);
        } else {
            assert!(
                open.is_empty(),
                "{status} cannot still add commits and must not block merge"
            );
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
fn merge_gate_reports_no_open_revisions_when_all_are_in_review_or_done() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "reviewed target");
    bind_open_pr(&db, &root.id);
    let first = apply_findings(&db, &root.id, "head-one");
    let second = apply_findings(&db, &root.id, "head-two");
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'in_review' WHERE id = ?1", [&first])
        .unwrap();
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'done' WHERE id = ?1", [&second])
        .unwrap();
    assert!(
        db.open_merge_revisions(&root.id).unwrap().is_empty(),
        "in_review and done revisions have already published and must not prompt"
    );
}

#[test]
fn merge_gate_reports_a_running_or_queued_revision() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "live target");
    bind_open_pr(&db, &root.id);
    let revision = apply_findings(&db, &root.id, "head-one");
    for status in ["todo", "active"] {
        db.connect()
            .unwrap()
            .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![revision, status])
            .unwrap();
        let open = db.open_merge_revisions(&root.id).unwrap();
        assert_eq!(open.len(), 1, "{status} can still add commits");
        assert_eq!(open[0].id, revision);
        assert_eq!(open[0].status, status);
    }
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

#[test]
fn untracked_and_deleted_findings_stay_open() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "untracked findings");
    bind_open_pr(&db, &root.id);
    let revision = apply_findings(&db, &root.id, "head-one");
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'done', deleted_at = 'now' WHERE id = ?1",
        [&revision],
    )
    .unwrap();
    drop(conn);
    assert_eq!(
        findings(&db, &root.id).unwrap().status_text,
        "1 finding: 0 fixed on PR, 0 in progress, 1 open"
    );
    db.connect()
        .unwrap()
        .execute(
            "UPDATE pr_review_verdicts SET revision_task_id = NULL WHERE revision_task_id = ?1",
            [&revision],
        )
        .unwrap();
    let untracked = findings(&db, &root.id).unwrap();
    assert_eq!(untracked.status_text, "1 finding: 0 fixed on PR, 0 in progress, 1 open");
    assert!(untracked.addendum_markdown.ends_with("- [high] Unchecked index"));
}
