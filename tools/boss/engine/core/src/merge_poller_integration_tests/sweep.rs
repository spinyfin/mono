use super::*;

/// A PR-review revision whose target merges while its worker is live must
/// cross execution families automatically: cancel the obsolete
/// revision_implementation, preserve the work item as an autostart followup,
/// and enqueue a chore_implementation without any manual start.
/// The cancellation must also close the original dispatch timeline.
#[tokio::test]
async fn merged_pr_redispatches_live_review_revision_as_followup_and_records_cancel() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/1501";
    let (product_id, parent_id) = make_chore_in_review(&db, "merged-review-followup", pr);
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address every review finding")
                .created_via(format!(
                    "{}exec_review_origin",
                    boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX
                ))
                .build(),
            &checker,
        )
        .unwrap();
    let original = db
        .list_executions(Some(&revision.id))
        .unwrap()
        .into_iter()
        .find(|execution| execution.kind == ExecutionKind::RevisionImplementation)
        .expect("revision creation must enqueue its implementation");
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&revision.id])
            .unwrap();
        conn.execute(
            "UPDATE work_executions
             SET status = 'running', cube_lease_id = 'lease-review',
                 cube_workspace_id = 'workspace-review', workspace_path = '/tmp/workspace-review'
             WHERE id = ?1",
            [&original.id],
        )
        .unwrap();
    }

    let events = Arc::new(RecordingDispatchEventSink::new());
    let publisher = Arc::new(RecordingPublisher::default());
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        publisher.clone(),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
        crate::completion_adapters::completion_collaborators(),
    )
    .with_dispatch_events(events.clone());
    let probe = StubProbe::new();
    probe.set(pr, PrLifecycleState::Merged);

    let outcome = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;

    assert_eq!(outcome.merged, 1);
    assert_eq!(outcome.revision_invalidated, 1);
    assert_eq!(
        db.get_execution(&original.id).unwrap().status,
        ExecutionStatus::Cancelled
    );
    let followup = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(task) => task,
        other => panic!("expected converted followup, got {other:?}"),
    };
    assert_eq!(followup.kind, TaskKind::Followup);
    assert_eq!(followup.status, TaskStatus::Todo);
    assert!(followup.autostart);

    let executions = db.list_executions(Some(&revision.id)).unwrap();
    let redispatched: Vec<_> = executions
        .iter()
        .filter(|execution| execution.kind == ExecutionKind::ChoreImplementation)
        .collect();
    assert_eq!(redispatched.len(), 1, "followup must receive one fresh execution");
    assert_eq!(redispatched[0].status, ExecutionStatus::Ready);
    assert_eq!(
        redispatched[0].preferred_workspace_id.as_deref(),
        Some("workspace-review"),
        "followup must prefer the exact workspace assigned to the cancelled revision",
    );
    assert!(
        redispatched[0].allow_dirty,
        "preferred workspace must retain uncommitted edits"
    );
    assert!(
        redispatched[0].prefer_is_soft,
        "an unavailable preferred workspace must fall back to a fresh workspace",
    );
    assert_eq!(followup.product_id, product_id);

    let cancellation = events
        .events_for(&original.id)
        .await
        .into_iter()
        .find(|event| event.stage == "execution_cancelled")
        .expect("merge cancellation must be recorded in the dispatch timeline");
    assert_eq!(cancellation.outcome, "ok");
    assert_eq!(cancellation.details["reason"], "parent_pr_merged");
    assert_eq!(cancellation.details["prior_status"], "running");
    assert_eq!(cancellation.details["chain_root_id"], parent_id);
    assert!(
        crate::dispatch_reader::is_terminal_event(&cancellation),
        "merge cancellation must close the original dispatch timeline",
    );
}

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
        crate::completion_adapters::completion_collaborators(),
    );

    let outcome = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;

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
        crate::completion_adapters::completion_collaborators(),
    );

    let outcome = run_one_pass(
        db.as_ref(),
        probe.as_ref(),
        publisher.as_ref(),
        None,
        Some(&handler),
        Some(&core_host()),
    )
    .await;

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

/// When a PR develops both a merge conflict and a CI failure
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
        job_key: None,
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
    let out1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let out2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let out3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;

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
    // the worker-spawn path).
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(cube.as_ref() as &dyn CubeClient),
        None,
        Some(&core_host()),
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
        Some(&core_host()),
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

/// A
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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

/// PR#1861 end-to-end regression: a merge-queue rebounce whose fix
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
        boss_engine_merge_poller::queue_ci_contract::test_context(publisher.as_ref()),
        &rebounce_candidate,
        "original-pr-head",
        "synthetic-merge-sha",
        None,
        // A failing check from the synthetic merge commit: a queue-side
        // failure with none is refused outright (there would be nothing for
        // the fix revision to act on).
        &[RequiredCheckFailure {
            job_key: None,
            name: "ci/test".to_owned(),
            conclusion: "FAILURE".to_owned(),
            target_url: "https://buildkite.com/foo/bar/builds/42#job-uuid".to_owned(),
            provider: CiProvider::Buildkite,
            provider_job_id: Some("job-uuid".to_owned()),
        }],
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
    // now-stale base (the exact observed state from the PR#1861 regression).
    let probe = StubProbe::new();
    probe.states.lock().unwrap().insert(
        pr.into(),
        Ok(PrLifecycleProbe::builder()
            .url(pr)
            .state(PrLifecycleState::Open(OpenPrStatus {
                mergeability: OpenPrMergeability::Conflict,
                ci: OpenPrCiStatus::Clean,
            }))
            .base_ref_oid("main-sha-2")
            .head_ref_oid("rebased-head-sha")
            .head_ref_name("feature-branch")
            .base_ref_name("main")
            .labels(Vec::new())
            .review(PrReviewState::Unknown)
            .raw_mergeable("CONFLICTING")
            .raw_merge_state_status("DIRTY")
            .build()),
    );
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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

/// Drift-guard scenario integration test: worker B resolved against stale main
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;

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

    // The re-arm spawns an engine-triggered revision
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

/// A chore whose PR became
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
async fn stranded_blocked_parent_with_dirty_pr_regains_signal_and_respawns_revision() {
    // Regression for the PR #1077 strand, parameterised over both
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
        let o1 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
        let o2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let outcome2 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
    assert_eq!(outcome2.total_transitions(), 0);

    // Pass 3: probe flips to Clean; on_resolved retires the attempt and
    // clears the signal. Parent was already in_review — no status flip.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::clean()));
    let outcome3 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let outcome4 = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    // A successful push → next probe →
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
    // the worker-spawn path) so the next pass exercises the
    // attempt-aware retire path.
    probe.set(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only()));
    run_one_pass(
        &db,
        probe.as_ref(),
        publisher.as_ref(),
        Some(cube.as_ref() as &dyn CubeClient),
        None,
        Some(&core_host()),
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
        Some(&core_host()),
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
    let first = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
    assert_eq!(first.conflict_flagged, 1, "first pass must detect the conflict");
    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "first pass must create the conflict revision; got {attempts:?}"
    );
    assert!(
        attempts[0].revision_task_id.is_some(),
        "the conflict attempt must carry its revision task; got {attempts:?}",
    );
    match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => assert_eq!(t.status, TaskStatus::InReview),
        other => panic!("expected chore, got {other:?}"),
    }

    // Second pass: GitHub reports MERGED.
    probe.set(pr, PrLifecycleState::Merged);
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    // Sweep-level end-to-end: a labelled PR
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

    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;

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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;

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

/// When both
/// `merge_conflict` and `ci_failure` rows are active in the side
/// table, the polymorphic dispatch iterates both. Only the signal
/// whose probe condition holds clears on a given pass; the other
/// stays active. Each clears
/// independently when its probe condition holds.
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
    let _ = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
    let outcome = run_one_pass(&db, probe.as_ref(), publisher.as_ref(), None, None, Some(&core_host())).await;
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
