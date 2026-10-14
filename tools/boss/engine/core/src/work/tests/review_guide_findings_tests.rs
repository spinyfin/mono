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
    db.apply_review_verdict_proposal(
        &outcome.proposal.id,
        &FakePrStateChecker::always(PrOpenState::Open).with_head_sha(sha),
    )
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

/// Seed a published guide at `head-one`, then the revision's `head-two`
/// capture with its own admitted (queued) incremental update attempt.
struct UpdatedGuide {
    revision: String,
    update_attempt: String,
}

fn seed_update(db: &WorkDb, root: &str, published_head: &str) -> UpdatedGuide {
    let revision = apply_findings(db, root, "head-one");
    let capture = |head, sequence| {
        let PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(
                root,
                sequence,
                PrSourceCaptureTrigger::Completion,
                &crate::test_support::source_capture_packet(PR_URL, "base", head),
            )
            .unwrap()
        else {
            panic!("capture");
        };
        capture
    };
    let old = capture("head-one", 1);
    let attempt = db
        .create_pr_review_guide_attempt(&old.series_id, &old.comparison_id, "test")
        .unwrap();
    db.publish_pr_review_guide_version(&attempt.id, "# Previous", "raw")
        .unwrap();
    db.record_review_guide_revision_head(root, PR_URL, "head-two", &revision, 2)
        .unwrap();
    let new = capture(published_head, 2);
    let attempt = db
        .create_automatic_pr_review_guide_attempt(&new.series_id, &new.comparison_id)
        .unwrap();
    UpdatedGuide {
        revision,
        update_attempt: attempt.id,
    }
}

fn set_status(db: &WorkDb, id: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![id, status])
        .unwrap();
}

fn publish_update(db: &WorkDb, guide: &UpdatedGuide) {
    db.publish_pr_review_guide_version(&guide.update_attempt, "# Updated", "raw")
        .unwrap();
}

const RESOLVED: &str = "resolved in published guide";

#[test]
fn done_fix_not_yet_in_the_published_guide_shows_the_live_badge_but_is_not_resolved() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "updated findings");
    bind_open_pr(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-two");
    // `archived` is neither a live fix nor resolved.
    set_status(&db, &guide.revision, "archived");
    let archived = findings(&db, &root.id).unwrap();
    assert!(!archived.addendum_markdown.contains("~~"));
    assert!(archived.status_text.contains("0 fixed on PR, 0 in progress, 1 open"));
    // Done, but the published guide still covers head-one.
    set_status(&db, &guide.revision, "done");
    let before = findings(&db, &root.id).unwrap();
    assert!(before.addendum_markdown.contains("- ✓ ~~[high] Unchecked index~~"));
    assert!(!before.addendum_markdown.contains(RESOLVED));
    assert!(!before.status_text.contains("resolved"));
    assert!(before.status_text.contains("1 fixed on PR, 0 in progress, 0 open"));
}

#[test]
fn done_revision_contained_in_the_published_head_is_resolved() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "updated findings");
    bind_open_pr(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-two");
    for status in ["in_review", "done"] {
        set_status(&db, &guide.revision, status);
        assert!(!findings(&db, &root.id).unwrap().addendum_markdown.contains(RESOLVED));
    }
    publish_update(&db, &guide);
    for status in ["in_review", "done"] {
        set_status(&db, &guide.revision, status);
        let after = findings(&db, &root.id).unwrap();
        assert!(
            after.addendum_markdown.contains("- ✓ ~~[high] Unchecked index~~"),
            "{status}"
        );
        assert!(after.addendum_markdown.contains(RESOLVED), "{status}");
        assert!(
            after
                .status_text
                .contains("1 fixed on PR, 0 in progress, 0 open (1 resolved in published guide)"),
            "{status}"
        );
    }
}

#[test]
fn reopened_revision_is_not_resolved_even_when_its_head_was_published() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "reopened findings");
    bind_open_pr(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-two");
    publish_update(&db, &guide);
    for status in ["todo", "active", "blocked"] {
        set_status(&db, &guide.revision, status);
        let text = findings(&db, &root.id).unwrap();
        assert!(!text.addendum_markdown.contains("~~"), "{status}");
        assert!(!text.addendum_markdown.contains(RESOLVED), "{status}");
        assert!(
            text.status_text.contains("0 fixed on PR, 1 in progress, 0 open"),
            "{status}"
        );
    }
}

#[test]
fn non_descendant_published_head_is_not_resolved_but_keeps_the_live_badge() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "force pushed findings");
    bind_open_pr(&db, &root.id);
    // The published head is not the head the revision produced, and no push
    // ancestry connects them (a force-push).
    let guide = seed_update(&db, &root.id, "head-three");
    set_status(&db, &guide.revision, "done");
    publish_update(&db, &guide);
    let text = findings(&db, &root.id).unwrap();
    assert!(text.addendum_markdown.contains("- ✓ ~~[high] Unchecked index~~"));
    assert!(!text.addendum_markdown.contains(RESOLVED));
    assert!(text.status_text.contains("1 fixed on PR, 0 in progress, 0 open"));
    assert!(!text.status_text.contains("resolved"));
}

#[test]
fn recorded_push_ancestry_lets_a_later_published_head_contain_the_fix() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "descendant findings");
    bind_open_pr(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-three");
    set_status(&db, &guide.revision, "done");
    publish_update(&db, &guide);
    assert!(!findings(&db, &root.id).unwrap().addendum_markdown.contains(RESOLVED));
    // A later revision pushed head-three on top of head-two.
    db.record_review_guide_head_ancestry(&root.id, PR_URL, "head-two", "head-three")
        .unwrap();
    assert!(findings(&db, &root.id).unwrap().addendum_markdown.contains(RESOLVED));
}

#[test]
fn update_waits_while_a_revision_in_the_chain_can_still_write() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "writer wait");
    bind_open_pr(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-two");
    let has_writer = |db: &WorkDb| {
        let conn = db.connect().unwrap();
        super::review_guide_jobs::guide_updates::chain_has_active_writer(&conn, &guide.update_attempt).unwrap()
    };
    set_status(&db, &guide.revision, "active");
    assert!(has_writer(&db));
    // Parked revisions are not writing.
    for status in ["todo", "blocked", "in_review", "done", "archived"] {
        set_status(&db, &guide.revision, status);
        assert!(!has_writer(&db), "{status}");
    }
}

fn attempt_execution(db: &WorkDb, attempt: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT execution_id FROM pr_review_guide_attempts WHERE id = ?1",
            [attempt],
            |row| row.get(0),
        )
        .unwrap()
}

fn give_root_a_repo(db: &WorkDb, root: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET repo_remote_url = 'https://github.com/acme/widget.git' WHERE id = ?1",
            [root],
        )
        .unwrap();
}

#[test]
fn dispatch_holds_the_update_while_a_revision_is_active_then_reconcile_binds_it() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "dispatch hold");
    bind_open_pr(&db, &root.id);
    give_root_a_repo(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-two");
    set_status(&db, &guide.revision, "active");
    db.dispatch_pr_review_guide_attempt(&guide.update_attempt, &root.id)
        .unwrap();
    assert_eq!(attempt_execution(&db, &guide.update_attempt), None);
    assert_eq!(db.reconcile_pr_review_guide_attempts().unwrap(), 1);
    assert_eq!(attempt_execution(&db, &guide.update_attempt), None);
    set_status(&db, &guide.revision, "in_review");
    db.reconcile_pr_review_guide_attempts().unwrap();
    assert!(attempt_execution(&db, &guide.update_attempt).is_some());
}

#[test]
fn a_parked_revision_does_not_hold_the_update() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "parked revision");
    bind_open_pr(&db, &root.id);
    give_root_a_repo(&db, &root.id);
    let guide = seed_update(&db, &root.id, "head-two");
    set_status(&db, &guide.revision, "blocked");
    db.dispatch_pr_review_guide_attempt(&guide.update_attempt, &root.id)
        .unwrap();
    assert!(attempt_execution(&db, &guide.update_attempt).is_some());
}

#[test]
fn poller_first_capture_is_held_then_becomes_the_incremental_update() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "poller first");
    bind_open_pr(&db, &root.id);
    give_root_a_repo(&db, &root.id);
    let revision = apply_findings(&db, &root.id, "head-one");
    let capture = |head, sequence| {
        let PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(
                &root.id,
                sequence,
                PrSourceCaptureTrigger::Completion,
                &crate::test_support::source_capture_packet(PR_URL, "base", head),
            )
            .unwrap()
        else {
            panic!("capture");
        };
        capture
    };
    let old = capture("head-one", 1);
    let first = db
        .create_pr_review_guide_attempt(&old.series_id, &old.comparison_id, "test")
        .unwrap();
    db.publish_pr_review_guide_version(&first.id, "# Previous", "raw")
        .unwrap();
    set_status(&db, &revision, "active");
    // The merge poller sees the pushed head before completion attribution.
    let new = capture("head-two", 2);
    let attempt = db
        .create_automatic_pr_review_guide_attempt(&new.series_id, &new.comparison_id)
        .unwrap();
    assert!(db.review_guide_comparison_has_attempt(&new.comparison_id).unwrap());
    db.dispatch_pr_review_guide_attempt(&attempt.id, &root.id).unwrap();
    assert_eq!(
        attempt_execution(&db, &attempt.id),
        None,
        "held while the revision writes"
    );
    db.record_review_guide_revision_head(&root.id, PR_URL, "head-two", &revision, 2)
        .unwrap();
    set_status(&db, &revision, "in_review");
    db.reconcile_pr_review_guide_attempts().unwrap();
    assert!(attempt_execution(&db, &attempt.id).is_some());
    let execution = attempt_execution(&db, &attempt.id).unwrap();
    let (old_head, markdown) = db
        .review_guide_update_context(&execution)
        .unwrap()
        .expect("incremental update");
    assert_eq!((old_head.as_str(), markdown.as_str()), ("head-one", "# Previous"));
}

#[test]
fn returning_to_an_earlier_head_enqueues_an_update_from_the_published_guide() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let root = create_test_chore_manual(&db, product.id, "a b a");
    bind_open_pr(&db, &root.id);
    let revision = apply_findings(&db, &root.id, "head-one");
    let capture = |head, sequence| {
        let PrSourceCapturePersistOutcome::Stored(capture) = db
            .persist_pr_review_guide_source_capture(
                &root.id,
                sequence,
                PrSourceCaptureTrigger::Completion,
                &crate::test_support::source_capture_packet(PR_URL, "base", head),
            )
            .unwrap()
        else {
            panic!("capture");
        };
        capture
    };
    let a = capture("head-a", 1);
    let first = db
        .create_pr_review_guide_attempt(&a.series_id, &a.comparison_id, "test")
        .unwrap();
    db.publish_pr_review_guide_version(&first.id, "# A", "raw").unwrap();
    let b = capture("head-b", 2);
    db.record_review_guide_revision_head(&root.id, PR_URL, "head-b", &revision, 2)
        .unwrap();
    let second = db
        .create_automatic_pr_review_guide_attempt(&b.series_id, &b.comparison_id)
        .unwrap();
    db.publish_pr_review_guide_version(&second.id, "# B", "raw").unwrap();
    // The revision rewrites the PR back to head A: a fresh comparison for it.
    assert!(!db.review_guide_comparison_has_attempt(&a.comparison_id).unwrap());
    assert!(db.review_guide_comparison_has_attempt(&b.comparison_id).unwrap());
}
