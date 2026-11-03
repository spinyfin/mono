use super::*;

#[tokio::test]
async fn merged_pr_is_promoted_and_publishes_invalidation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1";
    let (product_id, chore_id) = make_chore_in_review(&db, "C1", pr);

    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Merged);
    let publisher = Arc::new(RecordingPublisher::default());

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    assert!(
        !publisher.runtime_reasons().await.contains(&"pr_merged".to_owned()),
        "a merge moves the card between lanes: it must stay a full-board invalidation"
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.comments_reopened, 1);
    assert_eq!(outcome.merged, 0);
    assert_eq!(outcome.closed_unmerged, 1);

    let reloaded = db.get_comment(&comment.id).unwrap().unwrap();
    assert_eq!(reloaded.status, "active");
    assert!(reloaded.revise_task_id.is_none());
    assert!(
        reloaded.reopened_at.is_some(),
        "reopened comment must stamp reopened_at so the sidebar can render the Reopened chip"
    );

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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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

    let (outcome, tier) = reconcile_one(&db, probe.as_ref(), publisher.as_ref(), None, None, None, pr1).await;
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
        None,
        "https://github.com/foo/bar/pull/999",
    )
    .await;
    assert_eq!(outcome.total_transitions(), 0);
    assert_eq!(tier, None);
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome2.total_transitions(), 0, "idempotent re-probe must not re-fire",);

    // Pass 3: CI is clean. The blocked_ci slice picks the row up.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
    let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome3.ci_cleared, 1, "next clean probe must retire");
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::InReview);
            assert!(t.blocked_reason.is_none());
        }
        other => panic!("expected chore, got {other:?}"),
    }

    // Pass 4: idempotent retire.
    let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
///      prior fast-fail fix and `fast-check terminal fail + slow check
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
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let out3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }

    // Second pass: GitHub reports MERGED.
    probe.set(pr, PrLifecycleState::Merged);
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(outcome.conflict_flagged, 0);
    assert_eq!(outcome.total_transitions(), 0);
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }
    assert!(publisher.lifecycle_reasons().await.is_empty());
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
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    let ready = db.list_ready_executions().unwrap();
    assert!(
        ready.is_empty(),
        "failed crz must not be re-armed automatically; got {ready:?}",
    );
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let _ = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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

/// Once a fix push advances the PR head beyond the head attributed to the
/// queue event, the old queue attempt must retire before failure detection
/// evaluates the new head.
#[tokio::test]
async fn advanced_pr_head_retires_queue_attempt_and_new_failure_mints_again() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/2650";
    let (product_id, chore) = make_chore_in_review(&db, "C-queue-reaper", pr);
    let publisher = Arc::new(RecordingPublisher::default());
    let candidate = PendingMergeCheck {
        work_item_id: chore.clone(),
        product_id,
        pr_url: pr.to_owned(),
    };

    assert!(
        ci_watch::on_merge_queue_rebounce_detected(
            &db,
            crate::queue_ci_contract::test_context(publisher.as_ref()),
            &candidate,
            "original-pr-head",
            "synthetic-queue-commit",
            None,
            &[RequiredCheckFailure {
                job_key: None,
                name: "ci/test".to_owned(),
                conclusion: "FAILURE".to_owned(),
                target_url: "https://buildkite.com/foo/bar/builds/1".to_owned(),
                provider: CiProvider::Buildkite,
                provider_job_id: Some("job-old".to_owned()),
            }],
        )
        .await
    );
    let old_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("queue attempt");
    let old_revision = old_attempt.revision_task_id.clone().expect("queue fix revision");

    // The revision worker has finished its push. The attempt intentionally
    // remains pending until the merge poller observes the new PR head.
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute(
        "UPDATE work_executions SET status = 'completed' WHERE work_item_id = ?1",
        rusqlite::params![&old_revision],
    )
    .unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
        rusqlite::params![&old_revision],
    )
    .unwrap();

    let probe = StubProbe::new();
    probe.set_with_base_head(
        pr,
        PrLifecycleState::Open(OpenPrStatus {
            mergeability: OpenPrMergeability::Clean,
            ci: OpenPrCiStatus::Failing {
                failures: vec![RequiredCheckFailure {
                    job_key: None,
                    name: "ci/test".to_owned(),
                    conclusion: "FAILURE".to_owned(),
                    target_url: "https://buildkite.com/foo/bar/builds/2".to_owned(),
                    provider: CiProvider::Buildkite,
                    provider_job_id: Some("job-new".to_owned()),
                }],
            },
        }),
        "base-head",
        "new-pr-head",
    );
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(
        outcome.ci_flagged, 1,
        "the new failing head must mint another remediation"
    );

    let attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 2);
    assert!(attempts.iter().any(|a| {
        a.id == old_attempt.id
            && a.status == "abandoned"
            && a.failure_reason.as_deref() == Some("merge_queue_trigger_superseded")
    }));
    assert!(attempts.iter().any(|a| {
        a.head_sha_at_trigger == "new-pr-head"
            && a.failure_kind.as_deref() == Some("pr_branch_ci")
            && a.status == "pending"
    }));
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 2);
}

/// The production wiring seam for Trunk queue adoption: `sweep_one` must
/// actually call [`crate::trunk_queue_adopt::adopt_unattributed_trunk_queue_episode`]
/// for an open PR and count what it adopted.
///
/// Every other test of this feature calls the adopt function directly, so
/// none of them notices if the call is moved out of the `Open` arm or
/// dropped: the field would silently stay `None`, nothing would ever be
/// adopted, and the queue poller's candidate set would go back to "episodes
/// Boss started" — the original bug — with a green test suite.
#[tokio::test]
async fn sweep_adopts_a_trunk_queue_eviction_and_counts_it() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/brianduff/flunge/pull/1156";
    let (product_id, chore) = make_chore_in_review(&db, "C-trunk-adopt", pr);
    db.set_product_merge_mechanism(&product_id, Some("trunk_queue"))
        .unwrap();
    let publisher = Arc::new(RecordingPublisher::default());

    let probe = StubProbe::new();
    let mut evicted = PrLifecycleProbe::builder()
        .url(pr.to_owned())
        .state(PrLifecycleState::Open(OpenPrStatus::clean()))
        .head_ref_oid("a898daa34a0151810ec44b2d69722f5df21119dd")
        .base_ref_name("main")
        .labels(Vec::new())
        .review(PrReviewState::Unknown)
        .build();
    evicted.trunk_queue_check_failure = Some(TrunkQueueCheckFailure {
        name: "Trunk Merge Queue (main)".to_owned(),
        conclusion: "FAILURE".to_owned(),
        details_url: "https://app.trunk.io/flunge/merge-queue/c1478ade/1156".to_owned(),
        completed_at: Some(
            chrono::DateTime::from_timestamp(boss_engine_utils::epoch_time::now_epoch_secs() - 120, 0)
                .unwrap()
                .to_rfc3339(),
        ),
    });
    probe.states.lock().unwrap().insert(pr.into(), Ok(evicted));

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(
        outcome.trunk_episodes_adopted, 1,
        "the sweep must adopt the eviction, not just carry the field on the probe",
    );

    // The adopted row is what makes the episode enumerable by the Trunk
    // queue poller, which is the entire point of adopting.
    let listed = db.list_active_trunk_merge_intents().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].intent.work_item_id, chore);
    assert_eq!(listed[0].intent.target_branch, "main");
    assert!(
        listed[0].intent.adopted_at_head_sha.is_some(),
        "provenance: a non-NULL head sha distinguishes an adopted episode from a merge click",
    );

    // A second pass observes the same still-failed leaf and must not adopt
    // it again.
    let second = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    assert_eq!(second.trunk_episodes_adopted, 0);
    assert_eq!(db.list_active_trunk_merge_intents().unwrap().len(), 1);
}

// ── Bug B: late PR recovery ─────────────────────────────────────────────

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
        None,
    )
    .await;

    assert_eq!(
        outcome.late_pr_recovered, 0,
        "late_pr_recovered must be 0 when no handler is wired",
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
        None,
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

// ----- parse_dequeue_event_nodes (merge-queue reason case) -----

/// Acceptance test for the CI-status invalidation gap: once a
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
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    // The poll rewrites only this task's CI / review columns: runtime-class.
    assert!(
        publisher
            .runtime_reasons()
            .await
            .contains(&"pr_poll_state_updated".to_owned()),
        "pr_poll_state_updated must be a runtime-class invalidation"
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
    let out = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

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

/// `ci_flaky_retriggered` had no arm in `maybe_clear_blocked`'s dispatch: it
/// fell through to the `other =>` debug arm, which owns nothing and clears
/// nothing. When it was the ONLY active signal — a worker classified a
/// failure as infra, re-ran the build, and pushed nothing — no retire path
/// ever dispatched, so the flake tag stayed armed on the card after CI went
/// green. It rides alongside `ci_failure` in the common case, which is the
/// only reason the omission was survivable at all.
///
/// It belongs on the same axis as `ci_failure`: `ci_watch::on_ci_resolved` is
/// its retire path too, and the clear-side SQL
/// (`clear_ci_failure_signal_only`) already listed the reason.
#[tokio::test]
async fn a_lone_ci_flaky_retriggered_signal_is_cleared_when_ci_goes_green() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1153";
    let (product, chore) = make_chore_in_review(&db, "C-lone-flaky", pr);
    let probe = StubProbe::new();
    let publisher = Arc::new(RecordingPublisher::default());

    // A worker took the flake/infra exit on an ordinary per-PR CI failure:
    // the attempt goes terminal `retriggered` and stamps the signal.
    let attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.to_owned(),
            pr_number: 1153,
            head_branch: "feature".to_owned(),
            head_sha_at_trigger: "head-1".to_owned(),
            attempt_kind: "retrigger".to_owned(),
            consumes_budget: 0,
            failed_checks: "[]".to_owned(),
            failure_kind: "pr_branch_ci".to_owned(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("fresh attempt row");
    db.mark_ci_remediation_retriggered(&attempt.id)
        .unwrap()
        .expect("attempt flips to retriggered");

    let signals = db.active_blocked_signals(&chore).unwrap();
    assert_eq!(
        signals.iter().map(|s| s.reason.as_str()).collect::<Vec<_>>(),
        vec!["ci_flaky_retriggered"],
        "precondition: this is the ONLY active signal — the uncovered dispatch case",
    );

    // The re-run comes back green.
    probe
        .states
        .lock()
        .unwrap()
        .insert(pr.to_owned(), Ok(probe_ci_clean(pr, "head-1")));
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

    assert!(
        db.active_blocked_signals(&chore).unwrap().is_empty(),
        "a green re-run must retire the flake signal, not leave it armed forever: {:?}",
        db.active_blocked_signals(&chore).unwrap(),
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
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;

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
/// (a real repro path from the CI-status invalidation gap).
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
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, None).await;
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
// canonical parser's tests in `boss_github::pr_url`. `stored_pr_number` here
// is a thin `i64` adaptor over that helper for durable-row storage.
#[test]
fn stored_pr_number_adapts_to_i64() {
    assert_eq!(stored_pr_number("https://github.com/o/r/pull/123"), Some(123));
    assert_eq!(stored_pr_number("not a url at all"), None);
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
