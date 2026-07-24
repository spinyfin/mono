use super::*;

#[tokio::test]
async fn stalled_reviewer_fallback_refires_review_and_files_attention() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1766";
    let (product_id, task_id) = make_chore_active_with_dead_review(&db, "T2235", pr, "failed");

    let publisher = Arc::new(RecordingPublisher::default());
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let mut outcome = SweepOutcome::default();

    sweep_stalled_reviewer(
        &db,
        publisher.as_ref(),
        &task_id,
        &product_id,
        pr,
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_advanced, 1);
    assert_eq!(
        outcome.reviewer_fallback_review_refired, 1,
        "a dead (terminal, non-completed) pr_review execution must be re-enqueued immediately"
    );

    let item = db.get_work_item(&task_id).unwrap();
    match item {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }

    let executions = db.list_executions(Some(&task_id)).unwrap();
    assert!(
        executions
            .iter()
            .any(|e| e.kind == boss_protocol::ExecutionKind::PrReview && e.status == ExecutionStatus::Ready),
        "expected a fresh ready pr_review execution; got: {executions:?}"
    );

    let attentions = db.list_attention_items_for_work_item(&task_id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|a| a.kind == crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND),
        "expected a pr_review_died_without_findings attention item; got: {attentions:?}"
    );
}

#[tokio::test]
async fn stalled_reviewer_fallback_skips_refire_when_execution_still_running() {
    // Timeout sub-case: the pr_review execution is still nominally
    // `running` (wedged, not yet reaped). The fallback must still
    // unstick the kanban lane and file the attention item, but must
    // NOT double-dispatch a second reviewer on top of the still-live
    // (even if wedged) one.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1767";
    let (product_id, task_id) = make_chore_active_with_dead_review(&db, "T-timeout", pr, "running");

    let publisher = Arc::new(RecordingPublisher::default());
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let mut outcome = SweepOutcome::default();

    sweep_stalled_reviewer(
        &db,
        publisher.as_ref(),
        &task_id,
        &product_id,
        pr,
        &checker,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.reviewer_fallback_advanced, 1);
    assert_eq!(
        outcome.reviewer_fallback_review_refired, 0,
        "must not re-enqueue while the stale execution is still nominally live"
    );

    let attentions = db.list_attention_items_for_work_item(&task_id).unwrap();
    assert!(
        attentions
            .iter()
            .any(|a| a.kind == crate::pr_review_recovery::PR_REVIEW_DIED_ATTENTION_KIND),
        "attention item must still be filed even when re-fire is deferred"
    );
}

#[tokio::test]
async fn merged_pr_is_promoted_and_publishes_invalidation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1";
    let (product_id, chore_id) = make_chore_in_review(&db, "C1", pr);

    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Merged);
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.merged, 1);

    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Done);
            assert_eq!(t.pr_url.as_deref(), Some(pr));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let events = publisher.events.lock().await.clone();
    assert!(
        events
            .iter()
            .any(|(p, w, r)| p == &product_id && w == &chore_id && r == "pr_merged"),
        "expected pr_merged work-item event, got {events:?}",
    );
}

#[tokio::test]
async fn open_clean_pr_leaves_chore_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/2";
    let (_pid, chore_id) = make_chore_in_review(&db, "C2", pr);

    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.merged, 0);
    assert_eq!(outcome.conflict_flagged, 0);
    // No `blocked: merge_conflict` row in the corpus, so the clean
    // signal hits nothing on the resolve side either.
    assert_eq!(outcome.conflict_cleared, 0);
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
    // Only poll-state housekeeping events are allowed; no lifecycle flip.
    assert!(publisher.lifecycle_reasons().await.is_empty());
}

/// Comment-intent-classification design §"Reconciliation" (task 2c):
/// a comment addressed by this chore's `[Revise]` batch must reopen
/// when the chore's PR closes without merging — the minimal
/// comment-only hook that predates the full
/// `chore-lifecycle-pr-closed-unmerged` retire path. The reopen must
/// survive that retire firing in the same sweep: the chore's own
/// status transitions to `done` below, but the comment stays reopened
/// rather than being immediately re-resolved by the retire.
#[tokio::test]
async fn closed_unmerged_pr_reopens_addressed_comments() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3";
    let (product_id, chore_id) = make_chore_in_review(&db, "C3", pr);

    let comment = db
        .create_comment(CreateCommentInput {
            artifact_kind: "pr_doc".to_owned(),
            artifact_id: "pr_doc:git@github.com:foo/bar.git:branch:doc.md".to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: "x".to_owned(),
                prefix: String::new(),
                suffix: String::new(),
            },
            body: "please change x".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        })
        .unwrap();
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE work_comments SET status = 'in_revision', revise_task_id = ?1 WHERE id = ?2",
            rusqlite::params![chore_id, comment.id],
        )
        .unwrap();
    }

    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::ClosedUnmerged);
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.comments_reopened, 1);
    assert_eq!(outcome.merged, 0);
    assert_eq!(outcome.closed_unmerged, 1);

    let reloaded = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(reloaded.status, "active");
    assert!(reloaded.revise_task_id.is_none());

    // The chore itself is retired to `done` — a closed-unmerged PR is a
    // definitive human signal that this attempt is over
    // (`chore-lifecycle-pr-closed-unmerged.md`).
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
        other => panic!("expected chore, got {other:?}"),
    }

    let events = publisher.events.lock().await.clone();
    assert!(
        events
            .iter()
            .any(|(p, w, r)| p == &product_id && w == &chore_id && r == "comments_reopened_on_pr_closed_unmerged"),
        "expected comments_reopened_on_pr_closed_unmerged event, got {events:?}",
    );
}

/// `chore-lifecycle-pr-closed-unmerged.md`: a chore bound to a PR that
/// gets closed without merging must retire to `done` on the next
/// merge-poller tick — the on-close counterpart to
/// `merged_pr_is_promoted_and_publishes_invalidation` above. No redo
/// execution is spawned; PR closure is a definitive human signal.
#[tokio::test]
async fn closed_unmerged_pr_retires_chore_to_done() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/5";
    let (product_id, chore_id) = make_chore_in_review(&db, "C5", pr);

    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::ClosedUnmerged);
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.closed_unmerged, 1);
    assert_eq!(outcome.merged, 0, "closed-unmerged must not count as a merge");

    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Done);
            assert_eq!(t.pr_url.as_deref(), Some(pr));
        }
        other => panic!("expected chore, got {other:?}"),
    }

    let events = publisher.events.lock().await.clone();
    assert!(
        events
            .iter()
            .any(|(p, w, r)| p == &product_id && w == &chore_id && r == "pr_closed_unmerged"),
        "expected pr_closed_unmerged work-item event, got {events:?}",
    );

    // Idempotency: a second pass over the same (now-done) row must not
    // double-count or error, mirroring the merge path's idempotency.
    let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome2.closed_unmerged, 0);
}

#[tokio::test]
async fn probe_failure_does_not_crash_or_promote() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr_a = "https://github.com/foo/bar/pull/3";
    let pr_b = "https://github.com/foo/bar/pull/4";
    let (_pa, chore_a) = make_chore_in_review(&db, "Cerr", pr_a);
    let (_pb, chore_b) = make_chore_in_review(&db, "Cok", pr_b);

    let probe = StubProbe::new();
    probe.set_err(pr_a, "auth broken");
    probe.set(pr_b, PrLifecycleState::Merged);
    let publisher = Arc::new(RecordingPublisher::default());

    // The error on pr_a must not prevent pr_b from being promoted.
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.merged, 1);
    match db.get_work_item(&chore_a).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
    match db.get_work_item(&chore_b).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn merged_pr_promotes_project_task_to_done() {
    // Regression for the bug where the poller's SQL filter only
    // matched `kind = 'chore'`, leaving Performance project_tasks
    // stuck in `in_review` after their PRs landed (2026-05-07).
    // A `kind = 'project_task'` row with a merged PR must be
    // promoted by the same sweep that handles chores.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr_chore = "https://github.com/foo/bar/pull/100";
    let pr_proj = "https://github.com/foo/bar/pull/101";
    let (_pid_c, chore_id) = make_chore_in_review(&db, "Cmix", pr_chore);
    let (project_product_id, project_task_id) = make_project_task_in_review(&db, "PTmix", pr_proj);

    let probe = StubProbe::new();
    probe.set(pr_chore, PrLifecycleState::Merged);
    probe.set(pr_proj, PrLifecycleState::Merged);
    let publisher = Arc::new(RecordingPublisher::default());

    // Both kinds are mergeable, so a single sweep should promote
    // both rows — the project_task one being the regression case.
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        outcome.merged, 2,
        "merge poller must sweep both chore and project_task rows",
    );

    match db.get_work_item(&project_task_id).unwrap() {
        WorkItem::Task(t) => {
            assert_eq!(t.kind, TaskKind::ProjectTask);
            assert_eq!(t.status, TaskStatus::Done);
            assert_eq!(t.pr_url.as_deref(), Some(pr_proj));
        }
        other => panic!("expected project_task, got {other:?}"),
    }
    match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
        other => panic!("expected chore, got {other:?}"),
    }
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events
            .iter()
            .any(|(p, w, r)| p == &project_product_id && w == &project_task_id && r == "pr_merged"),
        "expected pr_merged work-item event for project_task, got {work_events:?}",
    );
}

#[tokio::test]
async fn unmerged_project_task_pr_stays_in_review() {
    // The same negative path as `open_clean_pr_leaves_chore_in_review`,
    // but for `kind = 'project_task'`. Guards against a future
    // change that filters back down to chores only.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/200";
    let (_pid, project_task_id) = make_project_task_in_review(&db, "PTopen", pr);

    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.total_transitions(), 0);
    match db.get_work_item(&project_task_id).unwrap() {
        WorkItem::Task(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected project_task, got {other:?}"),
    }
    assert!(publisher.lifecycle_reasons().await.is_empty());
}

#[tokio::test]
async fn empty_corpus_is_skipped() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    // No chores in review at all → no work, no errors, no events.
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.total_transitions(), 0);
    assert!(publisher.lifecycle_reasons().await.is_empty());
}

/// Adaptive-per-PR-timer follow-up (doc §9 item 3): `reconcile_one`
/// must scope the sweep to exactly the named PR, leaving every other
/// in-review candidate untouched even though it's probed as merged too.
#[tokio::test]
async fn reconcile_one_scopes_to_named_pr_only() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr1 = "https://github.com/foo/bar/pull/301";
    let pr2 = "https://github.com/foo/bar/pull/302";
    let (_p1, chore1) = make_chore_in_review(&db, "C301", pr1);
    let (_p2, chore2) = make_chore_in_review(&db, "C302", pr2);

    let probe = StubProbe::new();
    probe.set(pr1, PrLifecycleState::Merged);
    probe.set(pr2, PrLifecycleState::Merged);
    let publisher = Arc::new(RecordingPublisher::default());

    let (outcome, tier) = reconcile_one(&db, probe.as_ref(), publisher.as_ref(), None, None, pr1).await;
    assert_eq!(outcome.merged, 1);
    // Merged is a terminal state: the PR has just been transitioned out
    // of every candidate list, so `poll_tier_for_probe` returns `None`
    // and the caller drops it from the adaptive schedule instead of
    // spending another probe to re-confirm a fact that can't change.
    assert_eq!(tier, None);

    match db.get_work_item(&chore1).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
        other => panic!("expected chore, got {other:?}"),
    }
    // The other in-review PR must be untouched even though the stub
    // probe would also report it merged.
    match db.get_work_item(&chore2).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
}

/// A PR that isn't a live candidate on any of `reconcile_one`'s four
/// scoped lists (merged/closed/never known to this DB) must be a
/// pure no-op — no probe call, no tier, so the caller stops tracking
/// it until the next full sweep rediscovers it.
#[tokio::test]
async fn reconcile_one_returns_no_tier_for_unknown_pr() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    let (outcome, tier) = reconcile_one(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        None,
        None,
        "https://github.com/foo/bar/pull/999",
    )
    .await;
    assert_eq!(outcome.total_transitions(), 0);
    assert_eq!(tier, None);
}

#[tokio::test]
async fn sweep_drives_full_conflict_resolve_cycle() {
    // End-to-end through `run_one_pass`: conflict detected (parent stays
    // in Review — revision in Doing) → probe goes Clean → attempt retired.
    //
    // New-model behavior: parent never leaves in_review when a revision
    // fix vehicle is in flight. The poller picks the row up from the
    // in_review slice on every pass; blocked-conflict slice is empty.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/500";
    let (product, chore) = make_chore_in_review(&db, "Ccycle", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Pass 1: probe reports Conflict; revision spawned, parent stays in_review.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.conflict_flagged, 1);
    assert_eq!(outcome.conflict_cleared, 0);
    assert_eq!(outcome.merged, 0);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            // New-model: parent stays in_review while revision is in flight.
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }

    // Pass 2 with no change: idempotent — active revision already in flight,
    // pre-flight early-exit fires, zero transitions.
    let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome2.total_transitions(), 0);

    // Pass 3: probe flips to Clean; on_resolved retires the attempt and
    // clears the signal. Parent was already in_review — no status flip.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome3.conflict_cleared, 1);
    assert_eq!(outcome3.conflict_flagged, 0);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
            assert!(t.blocked_attempt_id.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Pass 4 with no change: clear is also idempotent.
    let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome4.total_transitions(), 0);

    // Event trail: conflict in flight → (no work-item event on resolve since
    // parent didn't change status). Poll-state events excluded.
    let reasons: Vec<String> = publisher
        .events
        .lock()
        .await
        .iter()
        .filter(|(p, w, r)| p == &product && w == &chore && r != "pr_poll_state_updated")
        .map(|(_, _, r)| r.clone())
        .collect();
    assert_eq!(
        reasons,
        vec!["conflict_revision_in_flight".to_owned()],
        "only conflict_revision_in_flight event expected (parent never blocked)",
    );
}

#[tokio::test]
async fn sweep_with_attempt_runs_retire_path_end_to_end() {
    // Phase 4 #10 acceptance: a successful push → next probe →
    // retire path runs end-to-end through `run_one_pass`. The
    // attempt row flips to `succeeded`, the parent goes back to
    // `in_review`, the cube lease is released, and the typed
    // ConflictResolutionSucceeded event lands on the product
    // topic.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/600";
    let (product, chore) = make_chore_in_review(&db, "C-attempt-cycle", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let cube = Arc::new(RecordingCubeClient::default());

    // Pass 1: flip to blocked. Then install the attempt (mirroring
    // Phase 3's worker-spawn path) so the next pass exercises the
    // attempt-aware retire path.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(cube.as_ref() as &dyn CubeClient),
        None,
    )
    .await;
    let attempt = db
        .insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 600,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base".into()),
            head_sha_before: Some("head".into()),
        })
        .unwrap()
        .unwrap();
    db.mark_conflict_resolution_running(&attempt.id, "lease-600", "ws-600", "worker-600")
        .unwrap();

    // Pass 2: probe flips to Clean. Retire runs.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome = run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(cube.as_ref() as &dyn CubeClient),
        None,
    )
    .await;
    assert_eq!(outcome.conflict_cleared, 1);

    // Parent in_review with blocked columns cleared.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
            assert!(t.blocked_attempt_id.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Attempt is succeeded.
    let attempt = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(attempt.status, "succeeded");
    assert!(attempt.finished_at.is_some());
    // Lease released exactly once.
    assert_eq!(
        cube.releases.lock().await.as_slice(),
        ["lease-600"],
        "retire path must release the attempt's cube lease through the poller",
    );
}

#[tokio::test]
async fn stranded_blocked_parent_with_dirty_pr_regains_signal_and_respawns_revision() {
    // Regression for the T795 / PR #1077 strand, parameterised over both
    // signal kinds so a conflict-only or ci-only regression cannot hide.
    //
    // Setup: the parent ran an earlier remediation (an attempt + revision
    // that resolved an earlier conflict/CI failure; that revision now sits
    // in `review`), then drifted into `status='blocked'` with a NULL scalar
    // `blocked_reason` and an EMPTY active-signal set — invisible to every
    // scalar-reason-keyed candidate list. Its PR is now dirty/red again.
    //
    // Expectation: the stranded-blocked reconciliation re-canonicalises the
    // parent back into the standard loop, re-arms the signal, and spawns a
    // FRESH revision — without disturbing the prior revision still in
    // `review`.
    for kind in [StrandKind::Conflict, StrandKind::Ci] {
        let reason = kind.reason();
        let dir = tempdir().unwrap();
        let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
        let pr = "https://github.com/foo/bar/pull/1077";
        let (_product, chore) = make_chore_in_review(&db, "Strand", pr);
        let probe = StubProbe::new();
        let publisher = Arc::new(RecordingPublisher::default());

        // --- Pass 1: the original conflict/CI failure spawns a revision. ---
        set_dirty_probe(&probe, pr, kind, "base-old", "head-old");
        let o1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        match kind {
            StrandKind::Conflict => assert_eq!(o1.conflict_flagged, 1, "{reason}: pass1"),
            StrandKind::Ci => assert_eq!(o1.ci_flagged, 1, "{reason}: pass1"),
        }

        // Capture the prior attempt + the revision it spawned.
        let (prior_attempt_id, prior_rev_id) = match kind {
            StrandKind::Conflict => {
                let a = db
                    .active_conflict_resolution_for_work_item(&chore)
                    .unwrap()
                    .expect("prior conflict_resolutions row");
                (a.id.clone(), a.revision_task_id.clone().expect("prior revision"))
            }
            StrandKind::Ci => {
                let a = db
                    .active_ci_remediation_for_work_item(&chore)
                    .unwrap()
                    .expect("prior ci_remediations row");
                (a.id.clone(), a.revision_task_id.clone().expect("prior revision"))
            }
        };

        // The earlier remediation resolved its conflict/failure: mark the
        // attempt succeeded and park the revision in `review` — the healthy
        // post-resolve shape (the revision is intentionally NOT advanced to
        // `done`).
        match kind {
            StrandKind::Conflict => {
                db.mark_conflict_resolution_succeeded(&prior_attempt_id, Some("head-resolved"))
                    .unwrap();
                db.clear_merge_conflict_signal_only(&chore).unwrap();
            }
            StrandKind::Ci => {
                db.mark_ci_remediation_succeeded(&prior_attempt_id, Some("head-resolved"))
                    .unwrap();
                db.clear_ci_failure_signal_only(&chore).unwrap();
                db.reset_ci_attempts_used(&chore).unwrap();
            }
        }
        db.update_work_item(
            &prior_rev_id,
            WorkItemPatch {
                status: Some("in_review".into()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();
        // The strand: the parent comes to rest `blocked` with a NULL reason
        // and no side-table signal.
        db.update_work_item(
            &chore,
            WorkItemPatch {
                status: Some("blocked".into()),
                blocked_reason: Some(String::new()),
                ..WorkItemPatch::default()
            },
        )
        .unwrap();

        // Precondition: stranded + invisible to the scalar-reason lists.
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Blocked, "{reason}: precondition status");
                assert!(t.blocked_reason.is_none(), "{reason}: precondition NULL reason");
            }
            other => panic!("{reason}: expected chore, got {other:?}"),
        }
        assert!(
            db.active_blocked_signals(&chore).unwrap().is_empty(),
            "{reason}: precondition empty signal set",
        );
        assert!(
            db.list_chores_blocked_on_merge_conflict().unwrap().is_empty()
                && db.list_chores_blocked_on_ci_failure().unwrap().is_empty(),
            "{reason}: invisible to the scalar-reason candidate lists",
        );
        assert_eq!(
            db.list_chores_stranded_blocked_remediation().unwrap().len(),
            1,
            "{reason}: stranded list must surface the orphan",
        );

        // --- Pass 2: PR is dirty/red again at a fresh base/head. ---
        set_dirty_probe(&probe, pr, kind, "base-new", "head-new");
        let o2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
        assert_eq!(
            o2.stranded_blocked_recanonicalized, 1,
            "{reason}: exactly one orphan recovered",
        );
        match kind {
            StrandKind::Conflict => assert_eq!(o2.conflict_flagged, 1, "{reason}: respawn"),
            StrandKind::Ci => assert_eq!(o2.ci_flagged, 1, "{reason}: respawn"),
        }

        // Invariant: the parent carries the blocking signal again and is no
        // longer stranded.
        assert!(
            db.active_blocked_signals(&chore)
                .unwrap()
                .iter()
                .any(|s| s.reason == reason),
            "{reason}: parent must carry the blocking signal after recovery",
        );
        assert!(
            db.list_chores_stranded_blocked_remediation().unwrap().is_empty(),
            "{reason}: parent recovered out of the stranded set",
        );

        // A FRESH revision spawned, distinct from the prior one — which
        // still sits in `review` (revisions are not auto-advanced to done).
        let new_rev = match kind {
            StrandKind::Conflict => {
                let rows = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
                assert_eq!(rows.len(), 2, "{reason}: a second attempt row for the re-dirty PR");
                rows.iter()
                    .filter_map(|r| r.revision_task_id.clone())
                    .find(|rid| rid != &prior_rev_id)
                    .expect("a new conflict revision must spawn")
            }
            StrandKind::Ci => {
                let rows = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
                assert_eq!(rows.len(), 2, "{reason}: a second attempt row for the re-dirty PR");
                rows.iter()
                    .filter_map(|r| r.revision_task_id.clone())
                    .find(|rid| rid != &prior_rev_id)
                    .expect("a new ci revision must spawn")
            }
        };
        assert_ne!(new_rev, prior_rev_id, "{reason}: new revision is distinct");
        match db.get_work_item(&prior_rev_id).unwrap() {
            WorkItem::Task(t) => {
                assert_eq!(
                    t.status,
                    TaskStatus::InReview,
                    "{reason}: prior revision stays in review"
                );
                assert_eq!(t.kind, TaskKind::Revision);
            }
            other => panic!("{reason}: expected prior revision task, got {other:?}"),
        }
        match db.get_work_item(&new_rev).unwrap() {
            WorkItem::Task(t) => {
                assert_eq!(t.kind, TaskKind::Revision, "{reason}: new fix vehicle is a revision");
                assert_ne!(
                    t.status,
                    TaskStatus::Done,
                    "{reason}: new revision is not auto-advanced"
                );
            }
            other => panic!("{reason}: expected new revision task, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn stranded_recovery_skips_dependency_gated_parent() {
    // A stranded `blocked: NULL` parent that is ALSO gated by an
    // unsatisfied prerequisite is left for the dependency-unblock sweep:
    // re-canonicalising it could lose the genuine dependency block when the
    // conflict later resolves. The remediation pass must not touch it.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/2077";
    let (product, chore) = make_chore_in_review(&db, "Gated", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Give the parent remediation ownership (a prior conflict revision).
    set_dirty_probe(&probe, pr, StrandKind::Conflict, "base-old", "head-old");
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    let crz = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("prior crz");
    db.mark_conflict_resolution_succeeded(&crz.id, Some("resolved"))
        .unwrap();
    db.clear_merge_conflict_signal_only(&chore).unwrap();

    // Add an unsatisfied gating prerequisite, then model the strand
    // (blocked with a NULL reason) co-occurring with the live dependency.
    let prereq = create_test_chore_manual(&db, product.clone(), "Prereq");
    db.add_dependency(AddDependencyInput {
        dependent: chore.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some(String::new()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    // It surfaces in the stranded list and is genuinely gated...
    assert_eq!(db.list_chores_stranded_blocked_remediation().unwrap().len(), 1);
    assert!(!db.gating_prereqs_for(&chore).unwrap().is_empty(), "gated precondition",);

    // ...so the recovery pass leaves it untouched even with a dirty PR.
    set_dirty_probe(&probe, pr, StrandKind::Conflict, "base-new", "head-new");
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        outcome.stranded_blocked_recanonicalized, 0,
        "gated parent must be skipped",
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Blocked);
            assert!(t.blocked_reason.is_none(), "left untouched at blocked: NULL");
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len(),
        1,
        "no fresh attempt spawned for a gated parent",
    );
}

#[tokio::test]
async fn stranded_list_excludes_parent_without_remediation_ownership() {
    // A `blocked: NULL` parent with a bound PR and an empty signal set but
    // NO conflict/ci remediation history is not the remediation flow's to
    // recover (it could be any non-remediation block); it must not surface.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/3077";
    let (_p, chore) = make_chore_in_review(&db, "NoRem", pr);
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some(String::new()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert!(
        db.list_chores_stranded_blocked_remediation().unwrap().is_empty(),
        "no remediation ownership ⇒ not a remediation orphan",
    );
}

#[tokio::test]
async fn recanonicalize_blocked_only_claims_null_reason_blocked_rows() {
    // The re-canonicalisation guard only re-claims a genuinely-orphaned
    // `blocked: NULL` row; it never disturbs an `in_review` row or
    // overwrites a foreign `blocked_reason` (e.g. `dependency`).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/4077";
    let (_p, chore) = make_chore_in_review(&db, "Recanon", pr);

    // in_review row → guard requires status='blocked' → no-op.
    assert!(
        db.recanonicalize_blocked_merge_conflict(&chore, pr).unwrap().is_none(),
        "must not claim an in_review row",
    );

    // blocked: dependency row → guard requires NULL reason → no-op.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some("dependency".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert!(
        db.recanonicalize_blocked_ci_failure(&chore, pr).unwrap().is_none(),
        "must not overwrite a foreign blocked_reason",
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.blocked_reason.as_deref(), Some("dependency")),
        other => panic!("expected chore, got {other:?}"),
    }

    // blocked: NULL row → claimed → merge_conflict + signal armed.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("blocked".into()),
            blocked_reason: Some(String::new()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    let updated = db
        .recanonicalize_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("claims the null-reason blocked row");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("merge_conflict"));
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "merge_conflict"),
        "signal armed on re-canonicalisation",
    );

    // pr_url mismatch → guard miss.
    assert!(
        db.recanonicalize_blocked_merge_conflict(&chore, "https://github.com/foo/bar/pull/999")
            .unwrap()
            .is_none(),
        "pr_url mismatch must miss the guard",
    );
}

#[tokio::test]
async fn sweep_drives_full_ci_failure_cycle() {
    // Phase 8 #22 acceptance: end-to-end through `run_one_pass`.
    // Pass 1: probe says CI failing → flip to blocked: ci_failure.
    // Pass 2: same probe (idempotent) → no transition.
    // Pass 3: probe flips to CI clean (after the worker pushed) →
    // retire path runs through the blocked_ci slice.
    // Pass 4: same retire (idempotent) → no transition.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/700";
    let (product, chore) = make_chore_in_review(&db, "Ccycle-ci", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Pass 1.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.ci_flagged, 1, "first sweep must run the CI remediation flow");
    assert_eq!(outcome.conflict_flagged, 0);
    // Unified in_review model (#1007 parity): the parent STAYS in_review
    // while the engine-triggered CI-fix revision is in flight; an active
    // `ci_failure` blocked-signal row keeps the retire path armed.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "an active ci_failure signal must keep the retire path armed",
    );

    // Pass 2: probe still reports the same failure.
    let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome2.total_transitions(), 0, "idempotent re-probe must not re-fire",);

    // Pass 3: CI is clean. The blocked_ci slice picks the row up.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
    let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome3.ci_cleared, 1, "next clean probe must retire");
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }

    // Pass 4: idempotent retire.
    let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome4.total_transitions(), 0);

    // Event trail: parent never enters `blocked` in the in_review model, so
    // only "ci_revision_in_flight" fires (pass 1). "ci_failure_resolved" is
    // NOT emitted when the parent stayed in_review throughout — symmetric
    // with the conflict path (merge_conflict_resolved is also suppressed
    // when the parent never blocked). Poll-state events excluded.
    let reasons: Vec<String> = publisher
        .events
        .lock()
        .await
        .iter()
        .filter(|(p, w, r)| p == &product && w == &chore && r != "pr_poll_state_updated")
        .map(|(_, _, r)| r.clone())
        .collect();
    assert_eq!(reasons, vec!["ci_revision_in_flight".to_owned()],);
}

/// When the CI-remediation attempt budget is exhausted, `run_one_pass`
/// must (1) flip the parent to `blocked: ci_failure_exhausted`, (2) emit
/// the `CiRemediationExhausted` frontend event, and (3) create a
/// work-item-scoped `ci_remediation_exhausted` attention item so the
/// operator knows automated remediation gave up and why.
#[tokio::test]
async fn budget_exhausted_surfaces_attention_item() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/888";
    let (_product_id, chore) = make_chore_in_review(&db, "C-exhaust", pr);

    // Pre-consume the default budget of 3 so the next detection
    // sees `used (3) >= budget (3)` and hits the exhausted path.
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE tasks SET ci_attempts_used = 3 WHERE id = ?1",
            rusqlite::params![chore],
        )
        .unwrap();
    }

    let probe = StubProbe::new();
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-exhaust")));
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        outcome.ci_flagged, 1,
        "budget-exhausted path still counts as a ci_flagged transition",
    );

    // Parent must be in blocked: ci_failure_exhausted.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Blocked);
            assert_eq!(
                t.blocked_reason.as_deref(),
                Some("ci_failure_exhausted"),
                "blocked_reason must be ci_failure_exhausted when budget is spent",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }

    // A work-item-scoped attention item must have been created.
    let items = db.list_attention_items_for_work_item(&chore).unwrap();
    assert_eq!(
        items.len(),
        1,
        "exactly one attention item should be filed on budget exhaustion",
    );
    let item = &items[0];
    assert_eq!(
        item.kind, "ci_remediation_exhausted",
        "attention item must carry the ci_remediation_exhausted kind",
    );
    assert!(
        item.work_item_id.as_deref() == Some(&chore),
        "attention item must be work-item-scoped (no execution_id)",
    );
    assert!(
        item.execution_id.is_none(),
        "attention item must not be bound to an execution",
    );
    assert!(
        item.body_markdown.contains(pr),
        "attention body must include the PR URL",
    );
    assert!(
        item.body_markdown.contains("ci/test"),
        "attention body must include the failing check name",
    );

    // The AttentionItemCreated frontend event must also have been emitted.
    let fe = publisher.typed_events.lock().await;
    let exhausted = fe
        .iter()
        .filter(|(_, e)| matches!(e, boss_protocol::FrontendEvent::CiRemediationExhausted { .. }))
        .count();
    assert_eq!(exhausted, 1, "CiRemediationExhausted event must be emitted");
    let attention_created = fe
        .iter()
        .filter(|(_, e)| matches!(e, boss_protocol::FrontendEvent::AttentionItemCreated { .. }))
        .count();
    assert_eq!(
        attention_created, 1,
        "AttentionItemCreated event must be emitted alongside the exhausted event",
    );
}

/// Drives the CI state machine through the full lifecycle and pins three
/// invariants:
///
///   1. PENDING != FAILING. A pure `InFlight` rollup (no failing leaf at
///      all) must NOT read as failing: no remediation revision spawned, no
///      `ci_failure` signal armed, `ci_attempts_used` stays 0, and the
///      persisted `ci_required_state` is `"in_progress"` — never `"fail"`.
///      (Note: a rollup with a *terminal* failing leaf + a still-running
///      check now correctly classifies as `Failing` immediately — see the
///      T1150 fast-fail fix and `fast-check terminal fail + slow check
///      running` matrix case. This test uses a pure in-flight probe with
///      no failing leaves at all.)
///   2. A `Failing` probe spawns exactly one remediation and the attempt
///      counter agrees (`ci_attempts_used == 1`, active attempt exists).
///      This is the accounting invariant the bug report saw violated
///      (counter 0 while a revision existed).
///   3. SUCCESS AFTER FAILING RECONCILES. A clean probe retires the
///      attempt, snaps the counter back to 0, and writes
///      `ci_required_state = "success"`.
#[tokio::test]
async fn inflight_ci_does_not_spawn_until_failure_is_terminal_then_reconciles() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1143";
    let (_product, chore) = make_chore_in_review(&db, "C-inflight-gate", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // --- Pass 1: CI is still running (InFlight) with NO failing leaf yet.
    // A pure all-in-progress rollup must not spawn a remediation. ---
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_in_flight(pr, "head-1")));
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        out1.ci_flagged, 0,
        "InFlight (non-terminal) CI must NOT spawn a remediation revision",
    );
    assert_eq!(
        out1.total_transitions(),
        0,
        "InFlight CI must not flip the parent or arm any signal",
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "parent stays in_review while CI runs");
            assert!(t.blocked_reason.is_none());
            assert_eq!(
                t.ci_required_state.as_deref(),
                Some("in_progress"),
                "pending != failing: an in-flight rollup persists as 'in_progress', never 'fail'",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        db.active_blocked_signals(&chore).unwrap().is_empty(),
        "no ci_failure signal may be armed while CI is non-terminal",
    );
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "no remediation attempt may exist for a non-terminal rollup",
    );
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        0,
        "the budget counter must not be consumed by an in-flight rollup",
    );

    // --- Pass 2: CI terminalizes to a genuine failure. NOW the spawn
    // gate is satisfied and exactly one remediation fires. ---
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        out2.ci_flagged, 1,
        "a terminal failed rollup must spawn exactly one remediation",
    );
    assert!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .any(|s| s.reason == "ci_failure"),
        "a terminal failure arms the ci_failure signal",
    );
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_some(),
        "a terminal failure creates an active remediation attempt",
    );
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        1,
        "the attempt counter must agree with the spawned revision (no 0-while-revision-exists drift)",
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(
            t.ci_required_state.as_deref(),
            Some("fail"),
            "a terminal failed rollup persists as 'fail'",
        ),
        other => panic!("expected chore, got {other:?}"),
    }

    // --- Pass 3: CI recovers to green. The attempt retires, the counter
    // resets, and the persisted state reconciles to success. ---
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
    let out3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(out3.ci_cleared, 1, "a clean probe must retire the remediation");
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
            assert_eq!(
                t.ci_required_state.as_deref(),
                Some("success"),
                "success after failing reconciles to success",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "the remediation attempt must be retired once CI is green",
    );
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        0,
        "a successful cycle resets the budget counter",
    );
}

#[tokio::test]
async fn list_chores_blocked_on_ci_failure_filters_correctly() {
    // Phase 8 #23 acceptance: the query returns only rows in
    // `blocked: ci_failure` or `ci_failure_exhausted` with a
    // `pr_url`, and excludes everything else (in_review,
    // blocked-on-other-reasons, soft-deleted, no-pr).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr_ci = "https://github.com/foo/bar/pull/800";
    let pr_exh = "https://github.com/foo/bar/pull/801";
    let pr_mc = "https://github.com/foo/bar/pull/802";
    let pr_ir = "https://github.com/foo/bar/pull/803";

    let (_p_ci, ci_chore) = make_chore_in_review(&db, "C-ci", pr_ci);
    let (_p_exh, exh_chore) = make_chore_in_review(&db, "C-exh", pr_exh);
    let (_p_mc, mc_chore) = make_chore_in_review(&db, "C-mc", pr_mc);
    let (_p_ir, _ir_chore) = make_chore_in_review(&db, "C-ir", pr_ir);

    db.mark_chore_blocked_ci_failure(&ci_chore, pr_ci, None).unwrap();
    db.mark_chore_blocked_ci_failure_exhausted(&exh_chore, pr_exh).unwrap();
    db.mark_chore_blocked_merge_conflict(&mc_chore, pr_mc).unwrap();

    let listed = db.list_chores_blocked_on_ci_failure().unwrap();
    let ids: std::collections::HashSet<String> = listed.iter().map(|c| c.work_item_id.clone()).collect();
    assert!(ids.contains(&ci_chore), "ci_failure row must be listed; got {ids:?}",);
    assert!(
        ids.contains(&exh_chore),
        "ci_failure_exhausted row must be listed; got {ids:?}",
    );
    assert!(
        !ids.contains(&mc_chore),
        "merge_conflict row must NOT be in the CI list; got {ids:?}",
    );
    // The in_review row stays out (it doesn't satisfy
    // `status='blocked'`).
    assert_eq!(listed.len(), 2, "exactly two CI-blocked rows should be returned",);
}

#[tokio::test]
async fn sweep_promotes_merged_pr_even_when_row_was_in_review_with_conflict() {
    // A row whose PR was force-merged while a conflict-resolution revision
    // was in flight should be promoted by the sweep. With the new model the
    // parent stays in_review (not blocked) while the revision is in Doing.
    // The Merged branch of the dispatch runs from the in_review candidate list.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/501";
    let (_product, chore) = make_chore_in_review(&db, "C-force-merged", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // First pass: conflict detected; parent stays in_review (revision spawned).
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }

    // Second pass: GitHub reports MERGED.
    probe.set(pr, PrLifecycleState::Merged);
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.merged, 1);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Done);
            assert_eq!(t.pr_url.as_deref(), Some(pr));
            assert!(
                t.blocked_reason.is_none(),
                "merging out of blocked must clear blocked_reason",
            );
            assert!(t.blocked_attempt_id.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Phase 6 #17 acceptance proxy: a chore whose PR became
/// conflicting while the engine was offline gets reconciled by
/// the first `run_one_pass` that runs at startup. The poller
/// already runs `run_one_pass` immediately on spawn (see
/// `spawn_loop`), so this test exercises the same path the
/// startup-sweep relies on: a single in-process `run_one_pass`
/// flips a pre-existing `in_review` row to `blocked: merge_conflict`
/// without any prior poller activity.
#[tokio::test]
async fn startup_sweep_picks_up_offline_conflict_transition() {
    // New-model: at startup, a CONFLICTING in_review PR spawns a revision
    // and the parent stays in_review (not blocked). The conflict_flagged
    // counter still increments (something happened).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/800";
    let (_product, chore) = make_chore_in_review(&db, "C-offline-conflict", pr);
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        outcome.conflict_flagged, 1,
        "startup sweep must pick up offline conflicts in one pass",
    );

    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            // New-model: parent stays in_review while revision is in flight.
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn startup_sweep_resolves_offline_clean_transition() {
    // Mirror case: a chore that was `blocked: merge_conflict`
    // before shutdown, whose PR is mergeable again at restart,
    // must retire on the first startup sweep.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/801";
    let (_product, chore) = make_chore_in_review(&db, "C-offline-clean", pr);
    // Put the row into blocked: merge_conflict directly so the
    // startup sweep has to drive the retire path on its first run.
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        outcome.conflict_cleared, 1,
        "startup sweep must retire offline-resolved conflicts in one pass",
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

#[tokio::test]
async fn opt_out_label_blocks_conflict_flip_through_sweep() {
    // Sweep-level end-to-end for Phase 6 #18: a labelled PR
    // reporting CONFLICTING leaves the chore in `in_review` and
    // records no transition.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/802";
    let (_product, chore) = make_chore_in_review(&db, "C-optout-sweep", pr);
    let probe = StubProbe::new();
    probe.set_with_labels(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        &["boss/no-auto-rebase"],
    );
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.conflict_flagged, 0);
    assert_eq!(outcome.total_transitions(), 0);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(publisher.lifecycle_reasons().await.is_empty());
}

/// T230 scenario integration test: worker B resolved against stale main
/// SHA (already-succeeded crz), but PR is still CONFLICTING. The next
/// merge-poller sweep must:
///   1. Detect the stale-base situation (succeeded crz + CONFLICTING PR).
///   2. Re-arm `task_blocked_signals`.
///   3. Dispatch a fresh crz against the new base SHA.
///   4. Leave all four state surfaces mutually consistent.
#[tokio::test]
async fn stale_base_succeeded_crz_rearmed_on_conflicting_pr() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/910";
    let (product, chore) = make_chore_in_review(&db, "C-t230", pr);

    // Simulate: conflict detected against old main SHA "sha-old".
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 910,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("sha-old".into()),
            head_sha_before: Some("sha-head-before".into()),
        })
        .unwrap()
        .expect("attempt insert must succeed");
    db.mark_conflict_resolution_running(&attempt.id, "lease-t230", "ws-t230", "worker-t230")
        .unwrap();

    // Worker B ran against the stale base and marked the crz succeeded.
    // (In the real scenario the task flip inside finalize_via_resolution_signal
    // missed due to blocked_attempt_id mismatch; here we reproduce the exact
    // wedged state: crz=succeeded, task=blocked:merge_conflict.)
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("sha-head-after"))
        .unwrap();
    // Ensure task is still blocked (the primary path's WHERE guard missed).
    let task = match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => t,
        other => panic!("expected Chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::Blocked);
    assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));

    // Probe now reports CONFLICTING against the *new* main SHA "sha-new".
    let probe = StubProbe::new();
    probe.set_with_base(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        Some("sha-new"),
    );
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

    // New-model: re-arm dispatches a fresh revision; the parent is
    // reconciled back to in_review. conflict_flagged = 1 because a state
    // transition occurred (blocked → in_review via reconcile path).
    assert_eq!(
        outcome.conflict_flagged, 1,
        "stale-base re-arm must count as a new event"
    );

    // A new crz must exist with base_sha_at_trigger = "sha-new".
    let crz_rows = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    let fresh_crz = crz_rows
        .iter()
        .find(|r| r.base_sha_at_trigger.as_deref() == Some("sha-new"))
        .unwrap_or_else(|| panic!("expected a fresh crz with base_sha_at_trigger=sha-new; rows={crz_rows:?}"));
    assert_eq!(fresh_crz.status, "pending", "fresh crz must be pending");

    // Phase 3 cutover: the re-arm spawns an engine-triggered revision
    // (not a bespoke conflict_resolution execution) as the fix vehicle.
    // The fresh crz carries the reverse link to that revision.
    let revision_task_id = fresh_crz
        .revision_task_id
        .as_deref()
        .expect("fresh crz must carry a revision_task_id after the re-arm cutover");
    let revision = match db.get_work_item(revision_task_id).unwrap() {
        crate::work::WorkItem::Task(t) => t,
        other => panic!("expected revision task, got {other:?}"),
    };
    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
    assert!(
        revision.created_via.starts_with("merge-conflict:"),
        "revision created_via must carry merge-conflict provenance; got {}",
        revision.created_via,
    );

    // The dormant conflict_resolution dispatch must NOT fire post-cutover.
    let ready = db.list_ready_executions().unwrap();
    assert!(
        !ready
            .iter()
            .any(|e| e.work_item_id == chore && e.kind == ExecutionKind::ConflictResolution),
        "cutover must not create a conflict_resolution execution; got {ready:?}",
    );

    // The original crz must still be succeeded.
    let orig = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(orig.status, "succeeded");

    // task_blocked_signals must have an active merge_conflict row.
    let signals = db.active_blocked_signals(&chore).unwrap();
    assert!(
        signals.iter().any(|s| s.reason == "merge_conflict"),
        "merge_conflict signal must be active after re-arm; got {signals:?}",
    );

    // New-model: parent is reconciled back to in_review (revision in flight).
    let task_after = match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => t,
        other => panic!("expected Chore, got {other:?}"),
    };
    assert_eq!(
        task_after.status,
        TaskStatus::InReview,
        "stale-base re-arm must reconcile parent to in_review (revision in flight)"
    );
    assert!(task_after.blocked_reason.is_none());
}

/// Complement test: a `failed` crz must NOT be re-armed (churn guard
/// and human own the retry). Verifies the stale-base path doesn't
/// widen to swallow the churn guard's intention.
#[tokio::test]
async fn failed_crz_is_not_rearmed_on_conflicting_pr() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/911";
    let (product, chore) = make_chore_in_review(&db, "C-failed-norearm", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product,
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 911,
            head_branch: "feature".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("sha-fail".into()),
            head_sha_before: None,
        })
        .unwrap()
        .expect("attempt insert must succeed");
    db.mark_conflict_resolution_failed(&attempt.id, "worker_died").unwrap();

    let probe = StubProbe::new();
    probe.set_with_base(
        pr,
        PrLifecycleState::Open(OpenPrStatus::conflict_only()),
        Some("sha-new"),
    );
    let publisher = Arc::new(RecordingPublisher::default());
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

    let ready = db.list_ready_executions().unwrap();
    assert!(
        ready.is_empty(),
        "failed crz must not be re-armed automatically; got {ready:?}",
    );
}

/// Drift-guard: when `task_blocked_signals` is empty but
/// `blocked_reason = 'merge_conflict'` and the probe returns Clean,
/// `maybe_clear_blocked` must still fire the retire path and flip the
/// task back to `in_review`.
#[tokio::test]
async fn drift_guard_clears_blocked_task_when_signals_empty_but_pr_clean() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/912";
    let (product, chore) = make_chore_in_review(&db, "C-drift-clean", pr);

    // Put the task into blocked:merge_conflict (signals + reason both set).
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();

    // Simulate the drift: clear the signal row manually without clearing
    // the blocked_reason on the tasks table.
    {
        let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
        conn.execute(
            "UPDATE task_blocked_signals SET cleared_at = '9999' WHERE work_item_id = ?1",
            [&chore],
        )
        .unwrap();
    }

    // Sanity: signal is now empty but blocked_reason is still set.
    assert!(db.active_blocked_signals(&chore).unwrap().is_empty());
    let task = match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => t,
        _ => panic!(),
    };
    assert_eq!(task.blocked_reason.as_deref(), Some("merge_conflict"));

    // Probe now returns Clean — the PR is mergeable.
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

    // The drift guard must have fired the retire path.
    assert_eq!(
        outcome.conflict_cleared, 1,
        "drift guard must clear the blocked task when signals empty and PR clean",
    );

    // Task must be back in_review.
    let task_after = match db.get_work_item(&chore).unwrap() {
        crate::work::WorkItem::Chore(t) => t,
        _ => panic!(),
    };
    assert_eq!(task_after.status, TaskStatus::InReview);
    assert!(task_after.blocked_reason.is_none());

    // work_item_changed event must have fired.
    let events = publisher.events.lock().await;
    assert!(
        events
            .iter()
            .any(|(pid, wid, r)| pid == &product && wid == &chore && r == "merge_conflict_resolved"),
        "expected merge_conflict_resolved event; got {events:?}",
    );
}

/// Phase 10 #31 acceptance (case 1 / merge_conflict alone): a
/// chore that carries only the `merge_conflict` signal in the
/// side table is routed to the conflict retire path by the
/// polymorphic dispatch (and crucially NOT to the CI retire
/// path). The `merge_conflict` row in `task_blocked_signals` is
/// stamped `cleared_at` once the conflict resolves.
#[tokio::test]
async fn polymorphic_clear_routes_merge_conflict_signal() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/910";
    let (_product_id, chore) = make_chore_in_review(&db, "C-mc-only", pr);

    // Stage merge_conflict only — mark_chore_blocked_merge_conflict
    // upserts the side-table row as part of the same transaction
    // (Phase 10 #31).
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let staged: Vec<String> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    assert_eq!(staged, vec!["merge_conflict".to_owned()]);

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Mergeable=Clean, CI=Clean — but the side table only has
    // merge_conflict, so the polymorphic dispatch must NOT fire
    // on_ci_resolved (which would have been a no-op anyway, but
    // the new shape skips the unconditional call entirely).
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.conflict_cleared, 1);
    assert_eq!(outcome.ci_cleared, 0);

    // Side table row was stamped `cleared_at`.
    let active = db.active_blocked_signals(&chore).unwrap();
    assert!(active.is_empty(), "merge_conflict signal cleared; got {active:?}");
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// mergeable=UNKNOWN must NOT retire a merge_conflict signal.
///
/// Root cause of the blocked↔in_review flap: GitHub returns `UNKNOWN`
/// transiently while recomputing mergeability (typically right after a
/// base-branch move or a poll that races the async recompute). Before
/// this fix, `UNKNOWN` was mapped to `Clean` and triggered
/// `conflict_watch::on_resolved`, unblocking the card. The next poll
/// read the definitive `CONFLICTING`/`DIRTY` and re-blocked it.
///
/// After the fix: `UNKNOWN` maps to `OpenPrMergeability::Unknown` and
/// the `merge_conflict` retire path is skipped. The card must stay
/// `blocked: merge_conflict` across the entire UNKNOWN poll. CI signals
/// are on a separate axis and are still processed (tested separately).
#[tokio::test]
async fn unknown_mergeability_does_not_retire_merge_conflict() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/912";
    let (_product_id, chore) = make_chore_in_review(&db, "C-unknown-mc", pr);

    // Manually install a blocked:merge_conflict signal (production
    // path: on_conflict_detected fires first, blocks the card).
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    {
        match db.get_work_item(&chore).unwrap() {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Blocked);
                assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Probe returns mergeable=UNKNOWN — GitHub is mid-recompute.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::unknown_mergeability()));
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

    // The merge_conflict retire path must NOT have fired.
    assert_eq!(
        outcome.conflict_cleared, 0,
        "UNKNOWN mergeability must not clear a merge_conflict signal"
    );
    assert_eq!(outcome.conflict_flagged, 0);

    // Card must still be blocked:merge_conflict.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(
                t.status,
                TaskStatus::Blocked,
                "card must remain blocked while mergeable=UNKNOWN"
            );
            assert_eq!(
                t.blocked_reason.as_deref(),
                Some("merge_conflict"),
                "blocked_reason must remain merge_conflict"
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }

    // No lifecycle event must have been emitted.
    assert!(
        publisher.lifecycle_reasons().await.is_empty(),
        "no lifecycle event expected while mergeable=UNKNOWN"
    );
}

/// Phase 10 #31/#32 acceptance (case 2 / ci_failure alone): a
/// chore that carries only the `ci_failure` signal is routed to
/// the CI retire path. Budget reset (#32) is observable: a chore
/// with `ci_attempts_used = 2` lands at 0 after the cycle.
#[tokio::test]
async fn polymorphic_clear_routes_ci_failure_signal_and_resets_budget() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/911";
    let (_product_id, chore) = make_chore_in_review(&db, "C-ci-only", pr);

    // Stage ci_failure only (the production detect path would do
    // this via `on_ci_failure_detected` → `mark_chore_blocked_ci_failure`).
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE tasks SET ci_attempts_used = 2 WHERE id = ?1",
            rusqlite::params![chore],
        )
        .unwrap();
    }
    let staged: Vec<String> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    assert_eq!(staged, vec!["ci_failure".to_owned()]);

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(outcome.ci_cleared, 1, "polymorphic dispatch fired on_ci_resolved");
    assert_eq!(
        outcome.conflict_cleared, 0,
        "no merge_conflict signal => no conflict retire"
    );

    let active = db.active_blocked_signals(&chore).unwrap();
    assert!(active.is_empty(), "ci_failure signal cleared; got {active:?}");
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert_eq!(t.ci_attempts_used, 0, "Phase 10 #32: full cycle resets budget to 0",);
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Phase 10 #31 acceptance (case 3 / both signals): when both
/// `merge_conflict` and `ci_failure` rows are active in the side
/// table, the polymorphic dispatch iterates both. Only the signal
/// whose probe condition holds clears on a given pass; the other
/// stays active. This mirrors the design's "each clears
/// independently when its probe condition holds" acceptance.
///
/// In production both signals being live simultaneously is rare
/// (the engine's compose-order Q1 has conflict pre-empt CI), but
/// the side-table can hold both rows for a window — e.g. when the
/// `ci_failure` row pre-dates a freshly-detected conflict — so
/// the dispatch's polymorphism must handle the case.
#[tokio::test]
async fn polymorphic_clear_each_signal_independent_when_both_active() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/912";
    let (_product_id, chore) = make_chore_in_review(&db, "C-both", pr);

    // Stage: the scalar `blocked_reason` lands on `ci_failure`
    // (its WHERE guard accepts `in_review`), and we hand-place a
    // sibling `merge_conflict` side-table row to simulate the
    // race window.
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO task_blocked_signals
                    (work_item_id, reason, attempt_id, created_at, cleared_at)
                 VALUES (?1, 'merge_conflict', NULL, '1700000000', NULL)",
            rusqlite::params![chore],
        )
        .unwrap();
    }
    let mut staged: Vec<String> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    staged.sort();
    assert_eq!(staged, vec!["ci_failure".to_owned(), "merge_conflict".to_owned()],);

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Pass 1: probe reports mergeable=Conflict, ci=Clean. The
    // dispatch must short-circuit before reaching either retire
    // path because `Conflict` mergeability routes to the
    // detect/idempotent path (not the Clean clear path). The
    // signals therefore stay active.
    probe.set(
        pr,
        PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Conflict,
            ci: OpenPrCiStatus::Clean,
        }),
    );
    let _ = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    let active_after_1: Vec<String> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    let mut active_after_1 = active_after_1;
    active_after_1.sort();
    assert_eq!(
        active_after_1,
        vec!["ci_failure".to_owned(), "merge_conflict".to_owned()],
        "Conflict mergeability must not clear either side-table row",
    );

    // Pass 2: probe reports mergeable=Clean, ci=Clean. The
    // dispatch's clean-branch iterates the side table and clears
    // the `merge_conflict` row (via on_resolved) and the
    // `ci_failure` row (via on_ci_resolved). Each fires
    // independently — neither hides the other.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    // The conflict retire path is no-op against the side-table row
    // because the scalar is `ci_failure`; the WHERE guard in
    // `clear_chore_blocked_merge_conflict` misses. However, the
    // signal-row clear happens regardless: the dispatch's
    // polymorphic iteration sees both reasons and routes
    // each — the CI retire fires (scalar matches), and the
    // conflict retire is a cheap no-op as designed.
    assert_eq!(outcome.ci_cleared, 1, "ci_failure retired (scalar matched ci_failure)",);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Phase 12 #41 — cross-flow ordering correctness. When a PR
/// develops both a merge conflict and a CI failure
/// simultaneously, the engine fires the conflict resolver first,
/// the CI fixer only after the conflict resolves. The
/// `task_blocked_signals` side table must reflect both signals
/// being active and clearing in the right order:
///
///   * Pass 1 (mergeable=Conflict + ci=Failing): `merge_conflict`
///     becomes active. CI detection is *not* invoked (the
///     mergeability=Conflict arm in `sweep_one` short-circuits
///     before reaching the Clean branch where ci_watch fires).
///   * Pass 2 (the worker has pushed; mergeable=Clean +
///     ci=Failing): the `merge_conflict` signal clears (probe
///     condition holds) and the `ci_failure` detect path runs in
///     the same sweep, adding `ci_failure` to the side table.
///   * Pass 3 (mergeable=Clean + ci=Clean): `ci_failure` clears
///     and the parent ends back at `in_review`.
#[tokio::test]
async fn cross_flow_conflict_then_ci_fires_in_order() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/941";
    let (_product_id, chore) = make_chore_in_review(&db, "C-cross", pr);

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let failures = vec![RequiredCheckFailure {
        name: "ci/test".into(),
        conclusion: "FAILURE".into(),
        target_url: "https://buildkite.com/anthropic/mono/builds/1#job".into(),
        provider: CiProvider::Buildkite,
        provider_job_id: Some("job-1".into()),
    }];

    // Pass 1: Conflict + Failing.
    let mut p1 = PrLifecycleProbe::builder()
        .url(pr)
        .state(PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Conflict,
            ci: OpenPrCiStatus::Failing {
                failures: failures.clone(),
            },
        }))
        .base_ref_oid("base-1")
        .head_ref_oid("head-1")
        .head_ref_name("feature")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build();
    probe.states.lock().unwrap().insert(pr.into(), Ok(p1.clone()));
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        out1.conflict_flagged, 1,
        "conflict_watch must fire first on Conflict+Failing",
    );
    assert_eq!(
        out1.ci_flagged, 0,
        "ci_watch must NOT fire while mergeability=Conflict (design §Q1)",
    );
    let active1: Vec<String> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    assert_eq!(active1, vec!["merge_conflict".to_owned()]);

    // Worker resolves the conflict — head sha advances and the
    // mergeability flips to Clean. CI is still failing on the new
    // head sha. (The conflict resolution attempt row is not
    // exercised here — we go straight to the next probe.)
    p1.state = PrLifecycleState::Open(OpenPrStatus {
        mergeability: OpenPrMergeability::Clean,
        ci: OpenPrCiStatus::Failing {
            failures: failures.clone(),
        },
    });
    p1.head_ref_oid = Some("head-2".into());
    probe.states.lock().unwrap().insert(pr.into(), Ok(p1.clone()));
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        out2.conflict_cleared, 1,
        "merge_conflict retire fires in the Clean branch",
    );
    assert_eq!(
        out2.ci_flagged, 1,
        "ci_watch detect fires in the same Clean sweep once conflict cleared",
    );
    let active2: Vec<String> = db
        .active_blocked_signals(&chore)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    assert_eq!(
        active2,
        vec!["ci_failure".to_owned()],
        "after pass 2, only ci_failure is active",
    );

    // Pass 3: CI goes green. The ci_failure signal retires and
    // the parent returns to `in_review`.
    p1.state = PrLifecycleState::Open(OpenPrStatus::clean());
    probe.states.lock().unwrap().insert(pr.into(), Ok(p1.clone()));
    let out3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(out3.ci_cleared, 1);
    assert!(db.active_blocked_signals(&chore).unwrap().is_empty());
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// T2381/PR#1861 end-to-end regression: a merge-queue rebounce whose fix
/// revision settles must not strand the parent invisibly in
/// `blocked: ci_failure`. Once its fix revision is spawned, the parent
/// must be back in `list_chores_pending_merge_check`'s `in_review`
/// bucket, so a later sweep that finds the PR CONFLICTING (green CI,
/// stale rebase base) routes straight to `conflict_watch` through the
/// normal `sweep_one` dispatch — exactly like any other open PR — instead
/// of being orphaned in ci_watch's bucket forever.
#[tokio::test]
async fn rebounce_settles_then_conflicting_base_rebuckets_via_sweep() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1861";
    let (product_id, chore) = make_chore_in_review(&db, "C-t2381-sweep", pr);
    let publisher = Arc::new(RecordingPublisher::default());

    // Step 1: simulate the merge-queue rebounce detection (in production
    // this comes from `check_merge_queue_rebounce`'s `gh` timeline probe;
    // called directly here, as ci_watch's own tests do, to seed the DB
    // state without a `gh` round-trip).
    let rebounce_candidate = crate::work::PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id: product_id.clone(),
        pr_url: pr.to_owned(),
    };
    let flipped = ci_watch::on_merge_queue_rebounce_detected(
        &db,
        publisher.as_ref(),
        &rebounce_candidate,
        Some("feature-branch"),
        "synthetic-merge-sha",
        &[],
        &[],
    )
    .await;
    assert!(flipped, "rebounce must be detected");

    // The fix revision spawns immediately, so the parent returns to
    // `in_review` (in_review model) rather than sitting in `blocked:
    // ci_failure` — this is the fix: it must land back in the sweep's
    // normal `in_review` probe pool.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview, "revision spawn must unblock the parent");
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }
    let in_review_bucket = db.list_chores_pending_merge_check().unwrap();
    assert!(
        in_review_bucket.iter().any(|c| c.work_item_id == chore),
        "parent must be visible in the in_review probe pool, not orphaned in ci_watch's bucket",
    );

    // Step 2: the revision force-pushed a rebase; the next sweep's probe
    // reports the PR's own CI as fully green but CONFLICTING against a
    // now-stale base (T2381's exact observed state).
    let probe = StubProbe::new();
    probe.states.lock().unwrap().insert(
        pr.into(),
        Ok(PrLifecycleProbe {
            url: pr.into(),
            state: PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Conflict,
                ci: OpenPrCiStatus::Clean,
            }),
            base_ref_oid: Some("main-sha-2".into()),
            head_ref_oid: Some("rebased-head-sha".into()),
            head_ref_name: Some("feature-branch".into()),
            base_ref_name: Some("main".into()),
            labels: Vec::new(),
            review: PrReviewState::Unknown,
            in_merge_queue: false,
            merge_queue_entry_state: None,
            merge_queue_position: None,
            merge_queue_enqueued_at: None,
            raw_mergeable: "CONFLICTING".into(),
            raw_merge_state_status: "DIRTY".into(),
            auto_merge_enabled: false,
            auto_merge_enabled_at: None,
        }),
    );
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(
        outcome.conflict_flagged, 1,
        "conflict_watch must fire via the normal in_review sweep dispatch",
    );

    // The stale merge_queue_rebounce ci_remediations attempt must not be
    // left active (it would otherwise strand a phantom "ci failing"
    // badge forever), and a conflict-resolution attempt must now exist.
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "stale ci_remediations attempt must be superseded",
    );
    let crz = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(crz.len(), 1, "a conflict-resolution attempt must be spawned");
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_ne!(
                t.blocked_reason.as_deref(),
                Some("ci_failure"),
                "row must no longer be stuck on the foreign ci_failure reason",
            );
        }
        other => panic!("expected chore, got {other:?}"),
    }
}

/// Cold-path regression pin: when a conflict-resolution worker pushes
/// a resolved branch but the engine's in-memory `StagedResolutionSignalCache`
/// is empty (e.g. engine restarted between the push and the Stop hook),
/// the merge-poller sweep must still detect the PR as mergeable and run
/// the retire path — transitioning the parent back to `in_review` and
/// marking the attempt `succeeded`.
///
/// This is the signal-missed recovery scenario that the primary-path
/// (on-Stop) shortcut cannot cover alone. The merge-poller sweep is the
/// structural fallback.
#[tokio::test]
async fn merge_poller_recovers_conflict_resolution_when_signal_missed() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/700";
    let (product, chore) = make_chore_in_review(&db, "C-signal-missed", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let cube = Arc::new(RecordingCubeClient::default());

    // Pass 1: flip to blocked, then install the attempt (mirroring
    // Phase 3's worker-spawn path).
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(cube.as_ref() as &dyn CubeClient),
        None,
    )
    .await;
    let attempt = db
        .insert_conflict_resolution(ConflictResolutionInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 700,
            head_branch: "feature-700".into(),
            base_branch: "main".into(),
            base_sha_at_trigger: Some("base-700".into()),
            head_sha_before: Some("head-700".into()),
        })
        .unwrap()
        .unwrap();
    db.mark_conflict_resolution_running(&attempt.id, "lease-700", "ws-700", "worker-700")
        .unwrap();

    // Simulate: the worker pushed and resolved the conflict but the
    // engine restarted — StagedResolutionSignalCache is empty and the
    // on-Stop primary path cannot fire. The PR is now MERGEABLE.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome = run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(cube.as_ref() as &dyn CubeClient),
        None,
    )
    .await;
    assert_eq!(
        outcome.conflict_cleared, 1,
        "merge-poller must recover the conflict transition when the signal was missed",
    );

    // Parent in_review.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }

    // Attempt succeeded.
    let after = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(after.status, "succeeded");
}

// ── Bug B: late PR recovery ─────────────────────────────────────────────

#[tokio::test]
async fn run_one_pass_recovers_late_pr_for_abandoned_execution() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (_, chore_id, _exec_id) = make_abandoned_chore_with_workspace(&db, "late-pr-sweep-chore");

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    let detector = Arc::new(FixedPrDetector(Some("https://github.com/foo/bar/pull/77".into())));
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        detector,
        Arc::new(NoopCubeClient),
        publisher.clone(),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
    );

    let outcome = run_one_pass(db.as_ref(), probe.as_ref(), publisher.as_ref(), None, Some(&handler)).await;

    assert_eq!(
        outcome.late_pr_recovered, 1,
        "expected one late PR recovery, got: {outcome:?}",
    );

    let task = match db.get_work_item(&chore_id).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert_eq!(task.pr_url.as_deref(), Some("https://github.com/foo/bar/pull/77"));
}

#[tokio::test]
async fn run_one_pass_does_not_query_late_pr_candidates_without_handler() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let (_product_id, chore_id, _exec_id) = make_abandoned_chore_with_workspace(&db, "late-pr-no-handler");

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Passing completion_handler = None; late-PR sweep should be skipped.
    // Also seed the in_review list so total > 0 and the sweep actually runs.
    let pr_url = "https://github.com/foo/bar/pull/78";
    db.update_work_item(
        &chore_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    probe.set(pr_url, PrLifecycleState::Open(OpenPrStatus::clean()));

    let outcome = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        None, // no handler
    )
    .await;

    assert_eq!(
        outcome.late_pr_recovered, 0,
        "late_pr_recovered must be 0 when no handler is wired",
    );
}

/// Issue #898: when the engine auto-transitions a `blocked: ci_failure`
/// task back to `in_review` (CI detected green), the live worker that
/// was running it must be force-stopped — it has nothing useful left
/// to do and otherwise holds its slot indefinitely. The task itself
/// stays in Review (force-stop's demotion guard only fires on
/// `active`).
#[tokio::test]
async fn ci_resolved_stops_live_worker_and_keeps_task_in_review() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/898";
    let (_product_id, chore, exec_id) = make_blocked_ci_chore_with_live_worker(&db, "C-898-stop", pr);

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));

    let handler = WorkerCompletionHandler::new(
        db.clone(),
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        publisher.clone(),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
    );

    let outcome = run_one_pass(db.as_ref(), probe.as_ref(), publisher.as_ref(), None, Some(&handler)).await;

    assert_eq!(outcome.ci_cleared, 1, "ci_failure retired to in_review");
    assert_eq!(
        outcome.worker_stopped_on_review, 1,
        "the live worker for the task was force-stopped, got: {outcome:?}",
    );

    // Task stays in Review — NOT demoted back to todo.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
    // The worker execution is now terminal and no longer live.
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Cancelled);
    assert!(
        db.get_live_execution_for_work_item(&chore, "").unwrap().is_none(),
        "no live worker should remain for the task",
    );
}

/// Without a completion handler wired (tests / cold-path), the CI
/// retire path still fires but the worker-stop is a no-op — the
/// counter stays 0 and the execution is left untouched.
#[tokio::test]
async fn ci_resolved_without_handler_does_not_stop_worker() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/899";
    let (_product_id, _chore, exec_id) = make_blocked_ci_chore_with_live_worker(&db, "C-898-nohandler", pr);

    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));

    let outcome = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        None, // no handler
    )
    .await;

    assert_eq!(outcome.ci_cleared, 1, "ci_failure still retires");
    assert_eq!(
        outcome.worker_stopped_on_review, 0,
        "no worker-stop without a handler, got: {outcome:?}",
    );
    // Execution untouched — still live.
    assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
}

// ----- parse_dequeue_event_nodes (merge-queue reason case T770/T771) -----

/// Acceptance test for T831 / the CI-status invalidation gap: once a
/// failure is recorded (`ci_required_state = "fail"`, `blocked: ci_failure`),
/// a subsequent clean probe must propagate the recovery transition — the
/// `blocked_ci` re-poll set must re-check the PR and update the task's
/// `ci_required_state` to `"success"` so the kanban CI indicator clears.
#[tokio::test]
async fn ci_required_state_clears_when_rollup_recovers_to_success() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/702";
    let (product, chore) = make_chore_in_review(&db, "C-ci-state-clear", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Pass 1: statusCheckRollup reports a FAILURE — simulates the initial
    // detection sweep that blocks the task.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(out1.ci_flagged, 1, "first sweep must detect and block on CI failure");

    // ci_required_state should reflect the failing rollup after detection.
    let ci_state_after_fail = match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => t.ci_required_state,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        ci_state_after_fail.as_deref(),
        Some("fail"),
        "ci_required_state must be 'fail' once the failing rollup is recorded",
    );

    // Pass 2: statusCheckRollup flips to SUCCESS — simulates CI recovering
    // (developer fixed the issue or flaky test re-ran green). The
    // blocked_ci re-poll set must re-check this PR and propagate the
    // recovery, clearing both the block and the CI indicator.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(out2.ci_cleared, 1, "clean probe must retire the ci_failure block");

    let t = match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(
        t.status,
        TaskStatus::InReview,
        "task must be back in_review after CI recovery"
    );
    assert!(t.blocked_reason.is_none(), "blocked_reason must be cleared");
    assert_eq!(
        t.ci_required_state.as_deref(),
        Some("success"),
        "ci_required_state must be 'success' after the rollup recovers — \
             this drives the PrCiIndicator green checkmark on the kanban card",
    );

    // A pr_poll_state_updated event must have been emitted so the macOS
    // kanban refreshes the CI indicator without waiting for a user action.
    let all_events = publisher.events.lock().await.clone();
    let has_poll_update = all_events
        .iter()
        .any(|(p, w, r)| p == &product && w == &chore && r == "pr_poll_state_updated");
    assert!(
        has_poll_update,
        "pr_poll_state_updated must be emitted when ci_required_state changes; \
             got: {all_events:?}",
    );

    // The retire path emits a clear event; the poll-state safety net may
    // also re-emit `CiFailureCleared` (idempotent). Either way the macOS
    // "ci failing" chip must receive at least one clear signal.
    assert!(
        publisher.ci_failure_cleared_count(pr).await >= 1,
        "a CiFailureCleared event must reach the UI when CI recovers to success",
    );
}

/// Issue #1151: a stale "ci failing" badge keyed to an earlier head must
/// be cleared by the state poll even when the engine has no active
/// blocked signal / remediation attempt to retire. This is the leak the
/// blocked-signal retire path (`maybe_clear_blocked` → `on_ci_resolved`)
/// does not cover: the chore sits `in_review` with a persisted
/// `ci_required_state = "fail"` left over from a prior commit's failing
/// poll, no `task_blocked_signals` row armed, yet the macOS card still
/// shows the "ci failing" chip. When the current head polls green the poll
/// must broadcast `CiFailureCleared` so the chip reconciles away.
#[tokio::test]
async fn stale_ci_failing_badge_cleared_by_poll_without_active_signal() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1151";
    let (product, chore) = make_chore_in_review(&db, "C-stale-badge", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Seed a stale failing poll-state directly — simulating the earlier
    // head's failing rollup having been recorded, while the engine's block
    // has since been quiesced (no active signal, status still in_review).
    let seeded = db
        .update_task_pr_poll_state(
            &chore,
            PrPollStateInput {
                ci_required_state: "fail",
                review_required_state: "unknown",
                ..Default::default()
            },
        )
        .unwrap();
    assert!(seeded.changed, "seed write must register a state change");
    assert!(
        db.active_blocked_signals(&chore).unwrap().is_empty(),
        "precondition: no active blocked signal — this is the uncovered leak path",
    );

    // Current head polls green.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-2")));
    let out = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

    // No retire path fired (there was nothing blocked to retire) — proving
    // the clear came from the poll-state safety net, not `on_ci_resolved`.
    assert_eq!(
        out.ci_cleared, 0,
        "no blocked-signal retire should fire; the clear must come from the poll",
    );

    // The persisted CI state must now read success.
    let ci_state = match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => t.ci_required_state,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(ci_state.as_deref(), Some("success"));

    // And the UI must have received the badge-clearing event.
    assert_eq!(
        publisher.ci_failure_cleared_count(pr).await,
        1,
        "the poll must broadcast exactly one CiFailureCleared on fail → success",
    );

    // Sanity: the event carries the right product/work-item identifiers.
    let events = publisher.typed_events.lock().await.clone();
    assert!(
        events.iter().any(|(_, e)| matches!(
            e,
            boss_protocol::FrontendEvent::CiFailureCleared { product_id: p, work_item_id: w, pr_url: u }
                if p == &product && w == &chore && u == pr
        )),
        "CiFailureCleared must carry the chore's product/work-item/pr ids; got: {events:?}",
    );
}

/// The poll-state safety net must NOT fire on a no-op clean poll: a chore
/// whose CI was already `success` (or never failing) must not emit a
/// spurious `CiFailureCleared` on every sweep. Only a genuine
/// `fail → success` transition clears the badge.
#[tokio::test]
async fn clean_poll_without_prior_failure_does_not_emit_clear() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1152";
    let (_product, _chore) = make_chore_in_review(&db, "C-no-spurious-clear", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
    // Two sweeps: the first writes success (prior NULL), the second is a
    // confirmed no-op. Neither saw a prior "fail" so neither may clear.
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;

    assert_eq!(
        publisher.ci_failure_cleared_count(pr).await,
        0,
        "a clean poll with no prior failing state must not emit CiFailureCleared",
    );
}

/// When a task is `blocked: ci_failure` at the time its PR is merged, any
/// pending `ci_remediations` rows must be abandoned so the macOS kanban
/// clears the "ci failing" badge. Without this cleanup the pending row
/// causes the badge to reappear on every `sendListCiRemediations` call
/// (T831 repro path).
#[tokio::test]
async fn merge_of_ci_blocked_pr_clears_badge() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/703";
    let (_product, chore) = make_chore_in_review(&db, "C-merge-clears-badge", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // Pass 1: CI fails — chore flips to blocked: ci_failure with a pending
    // ci_remediations row.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_failing(pr, "head-1")));
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(out1.ci_flagged, 1);
    // Unified in_review model: the parent stays in_review with a CI-fix
    // revision in flight; the pending ci_remediations row still exists and
    // is what drives the badge this test guards.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }

    // Verify the pending ci_remediations row exists (its presence is what
    // drives the badge via sendListCiRemediations).
    let active = db.active_ci_remediation_for_work_item(&chore).unwrap();
    assert!(
        active.is_some(),
        "a pending ci_remediations row must exist after detection"
    );

    // Pass 2: GitHub reports the PR as MERGED while CI is still failing on
    // the head branch (force-merge / merge-queue scenario). The sweep must
    // mark the pending row abandoned so it no longer shows up as
    // pending/running in the remediations list.
    probe.states.lock().unwrap().insert(
        pr.to_owned(),
        Ok(PrLifecycleProbe::builder()
            .url(pr.to_owned())
            .state(PrLifecycleState::Merged)
            .labels(vec![])
            .review(PrReviewState::Unknown)
            .build()),
    );
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None).await;
    assert_eq!(out2.merged, 1, "merge must be detected");

    // Task must be done.
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::Done),
        other => panic!("expected chore, got {other:?}"),
    }

    // The pending ci_remediations row must now be abandoned — a pending
    // row here would cause sendListCiRemediations to re-set the "ci
    // failing" badge on every app restart even though the task is done.
    let still_active = db.active_ci_remediation_for_work_item(&chore).unwrap();
    assert!(
        still_active.is_none(),
        "pending ci_remediations row must be abandoned on PR merge; \
             badge would persist on app restart otherwise",
    );
}

// NOTE: PR-number parsing behavior (standard URL, query/fragment stripping,
// trailing-path tolerance, and the strict rejections) is covered by the
// canonical parser's tests in `boss_github::pr_url`. `parse_pr_number` here
// is now a thin `i64` adaptor over `pr_number_from_url`.
#[test]
fn parse_pr_number_adapts_to_i64() {
    assert_eq!(parse_pr_number("https://github.com/o/r/pull/123"), Some(123));
    assert_eq!(parse_pr_number("not a url at all"), None);
}

#[test]
fn ci_state_str_maps_each_variant() {
    assert_eq!(ci_state_str(&OpenPrCiStatus::Clean), "success");
    assert_eq!(ci_state_str(&OpenPrCiStatus::InFlight), "in_progress");
    assert_eq!(ci_state_str(&OpenPrCiStatus::Failing { failures: vec![] }), "fail",);
}

#[test]
fn ci_detail_json_serializes_failing_checks() {
    let ci = OpenPrCiStatus::Failing {
        failures: vec![failure("ci/test", "FAILURE"), failure("ci/lint", "TIMED_OUT")],
    };
    let json = ci_detail_json(&ci).expect("non-empty failures → Some");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(
        parsed,
        serde_json::json!([
            {"name": "ci/test", "conclusion": "FAILURE"},
            {"name": "ci/lint", "conclusion": "TIMED_OUT"},
        ]),
    );
}

#[test]
fn ci_detail_json_none_when_failures_empty() {
    // Empty failure list → None so the DB column is NULL, not "[]".
    let ci = OpenPrCiStatus::Failing { failures: vec![] };
    assert_eq!(ci_detail_json(&ci), None);
}

#[test]
fn ci_detail_json_none_for_non_failing_variants() {
    assert_eq!(ci_detail_json(&OpenPrCiStatus::Clean), None);
    assert_eq!(ci_detail_json(&OpenPrCiStatus::InFlight), None);
}

#[test]
fn review_detail_json_none_when_empty() {
    assert_eq!(review_detail_json(&[]), None);
}

#[test]
fn review_detail_json_serializes_logins() {
    let reviewers = vec!["alice".to_owned(), "bob".to_owned()];
    let json = review_detail_json(&reviewers).expect("non-empty → Some");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(parsed, serde_json::json!(["alice", "bob"]));
}
