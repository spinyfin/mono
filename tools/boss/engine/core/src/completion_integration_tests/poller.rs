use super::*;
use boss_engine_completion::test_support::pr_transition::{PR, lifecycle_probe};
use boss_engine_completion::test_support::queue_ci_audit::{
    HEAD, input, rearm_and_notify, rearm_parent_failure_signal,
};

#[derive(Default)]
struct DeferredEnqueuer {
    calls: Mutex<Vec<(String, String, String, usize)>>,
}

#[async_trait]
impl ReviewBatchEnqueuer for DeferredEnqueuer {
    async fn enqueue(
        &self,
        _: &WorkDb,
        work_item_id: &str,
        repo: &str,
        pr: &str,
        pool: usize,
    ) -> Result<crate::work::ReviewBatchDispatch> {
        self.calls
            .lock()
            .await
            .push((work_item_id.into(), repo.into(), pr.into(), pool));
        Ok(crate::work::ReviewBatchDispatch::AdmissionDeferred)
    }
}

/// Regression for the missed PR-open detection that left chore
/// `task_18aefd1f955e5348_e` (PR #415) stuck in `active` with
/// `pr_url=NULL`. The on-Stop hook can miss a freshly-opened PR
/// when GitHub's `commits/{sha}/pulls` index hasn't caught up yet
/// (PR #415 was created 7s before the Stop fired). When that
/// happens the chore stays `active`, the merge poller's primary
/// query (`list_chores_pending_merge_check`) never picks it up
/// (that query gates on `status='in_review'`), and the chore is
/// stuck. The fix routes `waiting_human` executions with no
/// `pr_url` through `WorkerCompletionHandler::recheck_for_pr` on
/// every merge-poller pass, so a delayed GitHub-side propagation
/// recovers on the next 60s sweep.
#[tokio::test]
async fn merge_poller_recovers_missed_pr_open_for_waiting_human_execution() {
    use boss_observation_contracts::{MergeProbe, OpenPrStatus, PrLifecycleProbe, PrLifecycleState};

    // Fixture leaves the chore in `active` and the execution in
    // `waiting_human` with a workspace_path — exactly the state
    // PR #415 was in after its on-Stop hook missed.
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    // Simulate "on-Stop already ran and saw no PR" by leaving
    // the chore's pr_url unset. The recheck path is what we're
    // testing — it must see PrStatus::Fresh on this pass and
    // promote the chore.
    let workers_pr = "https://github.com/foo/bar/pull/415";
    let detector = StubPrDetector::ok(Some(workers_pr));
    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = engine_harness(db.clone(), detector);
    let handler = Arc::new(handler);

    // Wire a no-op MergeProbe — the test exercises only the
    // pending-PR-detection arm of the sweep, not the in-review
    // merge path.
    struct NoOpProbe;
    #[async_trait]
    impl MergeProbe for NoOpProbe {
        async fn probe(&self, _: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(String::new())
                .state(PrLifecycleState::Open(OpenPrStatus::clean()))
                .labels(Vec::new())
                .review(boss_observation_contracts::PrReviewState::Unknown)
                .build())
        }
    }
    let probe = NoOpProbe;

    let outcome = crate::merge_poller::run_one_pass(
        db.as_ref(),
        &probe,
        publisher.as_ref(),
        None,
        Some(handler.as_ref()),
        None,
    )
    .await;

    assert_eq!(
        outcome.pr_recheck_recovered, 1,
        "the sweep must recover exactly one missed PR-open transition, got {outcome:?}",
    );

    // chore held in `active` with pr_url stamped
    // (reviewer is enqueued to run the review pass).
    let item = db.get_work_item(&chore_id).unwrap();
    match item {
        WorkItem::Chore(t) => {
            assert_eq!(t.status, TaskStatus::Active);
            assert_eq!(t.pr_url.as_deref(), Some(workers_pr));
        }
        other => panic!("expected chore, got {other:?}"),
    }
    // Execution finalised — lease released, pane torn down.
    let execution = db.get_execution(&execution_id).unwrap();
    assert_eq!(execution.status, ExecutionStatus::Completed);
    assert!(execution.cube_lease_id.is_none());
    assert_eq!(
        cube.release_calls.lock().await.as_slice(),
        ["lease-1"],
        "the recovery must release the cube lease just like the on-Stop path does",
    );
    assert_eq!(
        pane.calls.lock().await.as_slice(),
        [execution_id.as_str()],
        "the recovery must tear down the pane just like the on-Stop path does",
    );
    // Crucially: NO probe was queued. Periodic polling must not
    // spam the worker's probe FIFO.
    assert!(
        probes.snapshot().is_empty(),
        "merge-poller recovery must NOT queue a probe — that's a Stop-event side effect only",
    );
    // Publish reason distinguishes recheck from on-Stop so
    // operators can see which path closed the chore.
    let work_events = publisher.events.lock().await.clone();
    assert!(
        work_events.iter().any(|(_, _, r)| r == "worker_pr_completed_recheck"),
        "expected worker_pr_completed_recheck publish reason, got {work_events:?}",
    );
}

#[tokio::test]
async fn merge_poller_recheck_binds_three_stuck_workers_when_detector_recovers() {
    use boss_observation_contracts::{MergeProbe, PrLifecycleProbe, PrLifecycleState};

    // Three independent workspaces / chores / executions in
    // `waiting_human` with `pr_url=null`. Mirrors the 3-worker
    // dispatch wave (Worf/Crusher/Troi).
    let ws1 = tempdir().unwrap();
    let ws2 = tempdir().unwrap();
    let ws3 = tempdir().unwrap();
    let (_dir, db, _p1, c1, e1) = fixture(ws1.path());
    // Reuse the same DB for the next two so a single merge-poller
    // pass sees all three executions.
    let chore2 = db
        .create_chore(
            crate::work::CreateChoreInput::builder()
                .product_id({
                    let item = db.get_work_item(&c1).unwrap();
                    item.product_id().to_string()
                })
                .name("Crusher")
                .build(),
        )
        .unwrap();
    let exec2 = create_ready_chore_execution(&db, chore2.id.clone());
    let (exec2, run2) = db
        .start_execution_run(
            &exec2.id,
            "worker-2",
            "mono",
            "lease-2",
            "mono-agent-002",
            ws2.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &exec2.id, &run2.id, None);
    let chore3 = db
        .create_chore(
            crate::work::CreateChoreInput::builder()
                .product_id({
                    let item = db.get_work_item(&c1).unwrap();
                    item.product_id().to_string()
                })
                .name("Troi")
                .build(),
        )
        .unwrap();
    let exec3 = create_ready_chore_execution(&db, chore3.id.clone());
    let (exec3, run3) = db
        .start_execution_run(
            &exec3.id,
            "worker-3",
            "mono",
            "lease-3",
            "mono-agent-003",
            ws3.path().to_str().unwrap(),
        )
        .unwrap();
    finish_run_worker_pane_alive(&db, &exec3.id, &run3.id, None);

    // Detector that returns Stale for every candidate — simulates
    // the failure mode where the worker's `@`/`@-` drifted from
    // the PR's head after push (`jj new main` after `jj git push`).
    // Keep a handle on the concrete stub so pass 2 can swap the
    // result without rebuilding the handler.
    let detector = StubPrDetector::ok_status(PrStatus::Stale {
        url: "https://github.com/spinyfin/mono/pull/433".into(),
        reason: "local commits do not match PR head abc1234".into(),
    });
    let TestHarness {
        handler,
        cube,
        publisher,
        pane,
        probes,
    } = engine_harness(db.clone(), detector.clone());

    struct NoOpProbe;
    #[async_trait]
    impl MergeProbe for NoOpProbe {
        async fn probe(&self, _: &str) -> anyhow::Result<PrLifecycleProbe> {
            Ok(PrLifecycleProbe::builder()
                .url(String::new())
                .state(PrLifecycleState::Open(boss_observation_contracts::OpenPrStatus::clean()))
                .labels(Vec::new())
                .review(boss_observation_contracts::PrReviewState::Unknown)
                .build())
        }
    }
    let probe = NoOpProbe;

    let outcome =
        crate::merge_poller::run_one_pass(db.as_ref(), &probe, publisher.as_ref(), None, Some(&handler), None).await;

    // Pass 1 — pre-fix behaviour: the recheck reaches all three
    // candidates but the detector still returns Stale on each. The
    // observability counter fires so the failure leaves a
    // breadcrumb on every sweep, but nothing transitions — exactly
    // the 2026-05-13 stuck-worker shape.
    assert_eq!(
        outcome.pr_recheck_unresolved, 3,
        "the sweep must count three unresolved recheck candidates, got {outcome:?}",
    );
    assert_eq!(
        outcome.pr_recheck_recovered, 0,
        "no transitions happen on the StalePr branch",
    );
    for chore_id in [c1.as_str(), chore2.id.as_str(), chore3.id.as_str()] {
        let item = db.get_work_item(chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(t.status, TaskStatus::Active);
                assert!(t.pr_url.is_none());
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }
    for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
        let execution = db.get_execution(execution_id).unwrap();
        assert_eq!(
            execution.status,
            ExecutionStatus::Running,
            "execution must stay live (running) after a Stale recheck",
        );
        assert!(
            execution.cube_lease_id.is_some(),
            "cube lease must NOT be released on the Stale branch — the worker is still alive",
        );
    }
    assert!(
        probes.snapshot().is_empty(),
        "recheck path stays quiet on Stale — no probes queued",
    );

    // Pass 2 — fix engaged: simulate the production path where
    // `jj_candidate_commit_shas`'s `committer_date(after:"<started_at>")`
    // gate has now expanded the candidate set with the worker's
    // pushed bookmark tip, so `gh api commits/{sha}/pulls` returns
    // a PR whose `head.sha` matches a local sha and `classify_pr`
    // accepts it as `Fresh`. The stub detector swaps to mirror
    // that real-world transition. All three stuck chores must
    // bind their `pr_url` and transition to `in_review` on this
    // pass — without coordinator backfill. Branch-keyed detection
    // returns each execution's OWN PR: three branches, three PRs (one
    // PR bound to three work items would be refused by the foreign-PR
    // guard, exactly as it should be).
    let recovered: Vec<(&str, &str, String)> = [
        (c1.as_str(), e1.as_str(), 433),
        (chore2.id.as_str(), exec2.id.as_str(), 434),
        (chore3.id.as_str(), exec3.id.as_str(), 435),
    ]
    .into_iter()
    .map(|(chore_id, execution_id, pr_number)| {
        let url = format!("https://github.com/spinyfin/mono/pull/{pr_number}");
        detector.set_fresh_for_branch(
            &expected_branch_name(execution_id, &BranchNaming::BossExecPrefix, None),
            &url,
        );
        (chore_id, execution_id, url)
    })
    .collect();
    let outcome2 =
        crate::merge_poller::run_one_pass(db.as_ref(), &probe, publisher.as_ref(), None, Some(&handler), None).await;
    assert_eq!(
        outcome2.pr_recheck_recovered, 3,
        "all three stuck workers must transition on the recovery pass, got {outcome2:?}",
    );
    assert_eq!(
        outcome2.pr_recheck_unresolved, 0,
        "no candidates should remain unresolved after the recovery pass",
    );
    // chore_implementation holds tasks in `active` while
    // reviewers are enqueued (not advanced to in_review yet).
    for (chore_id, _execution_id, own_pr_url) in &recovered {
        let item = db.get_work_item(chore_id).unwrap();
        match item {
            WorkItem::Chore(t) => {
                assert_eq!(
                    t.status,
                    TaskStatus::Active,
                    "chore {chore_id} must be held in active (reviewer enqueued)",
                );
                assert_eq!(
                    t.pr_url.as_deref(),
                    Some(own_pr_url.as_str()),
                    "chore {chore_id} must have its OWN pr_url bound on the recovery pass",
                );
            }
            other => panic!("expected chore, got {other:?}"),
        }
    }
    for execution_id in [e1.as_str(), exec2.id.as_str(), exec3.id.as_str()] {
        let execution = db.get_execution(execution_id).unwrap();
        assert_eq!(
            execution.status,
            ExecutionStatus::Completed,
            "execution {execution_id} must finalise on the recovery pass",
        );
        assert!(
            execution.cube_lease_id.is_none(),
            "cube lease must be released on the recovery pass — worker has stopped",
        );
    }
    // Three leases released, three panes torn down: one per worker.
    assert_eq!(cube.release_calls.lock().await.len(), 3);
    assert_eq!(pane.calls.lock().await.len(), 3);
    // No probes queued even on the success path — recheck never
    // probes (that's a Stop-event-only side effect).
    assert!(probes.snapshot().is_empty());
}

#[tokio::test]
async fn background_children_horizon_is_rechecked_without_another_stop() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, _chore_id, execution_id) = fixture(workspace.path());
    let detector = StubPrDetector::ok(None);
    let TestHarness {
        handler,
        publisher,
        probes,
        ..
    } = engine_harness(db.clone(), detector);
    let handler = handler.with_background_activity_probe(Arc::new(FixedDescendantProbe(1)));

    let first = handler.on_stop(&execution_id).await;
    assert!(
        matches!(first, StopOutcome::BackgroundChildrenPending { .. }),
        "the Stop must establish a pending background-child nudge; got {first:?}",
    );
    assert!(probes.snapshot().is_empty());

    // No second `on_stop` call: change only the horizon and drive the same
    // recurring pass production uses. The saved Stop intent must be replayed.
    let handler = handler.with_background_children_horizon_secs(0);
    crate::merge_poller::run_one_pass(
        db.as_ref(),
        &NoopMergeProbe,
        publisher.as_ref(),
        None,
        Some(&handler),
        None,
    )
    .await;

    let queued = probes.snapshot();
    assert_eq!(queued, [(execution_id, PROBE_NO_PR.to_owned())]);
}

#[tokio::test]
async fn run_one_pass_uses_injected_enqueuer_for_pending_review() {
    let (dir, db) = open_db_arc();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Deferred review");
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'active', pr_url = ?2 WHERE id = ?1",
            rusqlite::params![chore.id, PR],
        )
        .unwrap();
    file_admission_deferred_attention(&db, &chore.id, PR);
    assert_eq!(db.list_tasks_awaiting_pre_merge_review_admission().unwrap().len(), 1);
    let flags = Arc::new(crate::feature_flags::FeatureFlagsStore::new(
        dir.path().join("flags.toml"),
    ));
    flags.load().unwrap();
    flags.set("review_batch_fanout", true).unwrap();
    let enqueuer = Arc::new(DeferredEnqueuer::default());
    // Arc forwarding is the same path used by the production scheduling loop.
    let handler = Arc::new(
        engine_integration_handler(db.clone(), PR)
            .with_feature_flags(flags)
            .with_review_pool_size(3)
            .with_review_batch_enqueuer(enqueuer.clone()),
    );
    let publisher = RecordingPublisher::default();
    let outcome = crate::merge_poller::run_one_pass(&db, &NoopMergeProbe, &publisher, None, Some(&handler), None).await;
    assert_eq!(
        enqueuer.calls.lock().await.as_slice(),
        &[(chore.id, product.repo_remote_url.unwrap(), PR.into(), 3)]
    );
    assert_eq!(outcome.review_admission_still_deferred, 1);
}

#[tokio::test]
async fn poll_state_adapter_persists_and_publishes_like_direct_update() {
    let (_dir, db) = open_db();
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Poll-state adapter");
    let candidate = PendingMergeCheck {
        work_item_id: chore.id.clone(),
        product_id: product.id.clone(),
        pr_url: PR.into(),
    };
    let publisher = Arc::new(RecordingPublisher::default());
    let adapter = crate::merge_poller::MergePollerPrPollStateWriter::new(publisher.clone());
    let probe = lifecycle_probe();
    adapter
        .update_pr_poll_state(&db, &candidate, &probe, "completion")
        .await;
    let adapted = db.get_work_item(&chore.id).unwrap();
    assert_eq!(publisher.runtime_reasons().await, ["pr_poll_state_updated"]);
    assert_eq!(
        publisher.events.lock().await.as_slice(),
        &[(product.id.clone(), chore.id.clone(), "pr_poll_state_updated".into())]
    );
    let state = serde_json::to_value(&adapted).unwrap();
    assert_eq!(state["ci_required_state"], "success");
    assert_eq!(state["review_required_state"], "required");
    // Replaying the same observation must not emit another invalidation.
    adapter
        .update_pr_poll_state(&db, &candidate, &probe, "completion")
        .await;
    assert_eq!(publisher.events.lock().await.len(), 1);
    db.connect().unwrap().execute(
            "UPDATE tasks SET ci_required_state = NULL, review_required_state = NULL, pr_mergeable_state = NULL, ci_required_detail = NULL, review_required_detail = NULL WHERE id = ?1", [&chore.id],
        ).unwrap();
    publisher.events.lock().await.clear();
    crate::merge_poller::update_pr_poll_state(&db, publisher.as_ref(), &candidate, &probe, None, "completion").await;
    let direct = serde_json::to_value(db.get_work_item(&chore.id).unwrap()).unwrap();
    for field in [
        "ci_required_state",
        "review_required_state",
        "pr_mergeable_state",
        "ci_required_detail",
        "review_required_detail",
        "merge_queue_state",
        "merge_queue_detail",
    ] {
        assert_eq!(state[field], direct[field], "{field}");
    }
    assert_eq!(
        publisher.events.lock().await.as_slice(),
        &[(product.id, chore.id, "pr_poll_state_updated".into())]
    );
}

#[tokio::test]
async fn rearm_restores_the_parent_signal_not_the_revisions() {
    let (_dir, db) = crate::test_support::open_db();
    let product = crate::test_support::create_test_product(&db);
    let parent = crate::test_support::create_test_chore(&db, product.id.clone(), "Parent");
    let revision = crate::test_support::create_test_chore(&db, product.id.clone(), "CI fix");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET status = 'blocked', blocked_reason = 'ci_failure' WHERE id = ?1",
            [&parent.id],
        )
        .unwrap();
    }
    let mut audit = input(&parent.id, true);
    audit.attempt = db
        .insert_ci_remediation(crate::work::CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: parent.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/1".into(),
            pr_number: 1,
            head_branch: "feature".into(),
            head_sha_at_trigger: format!("mq:{HEAD}@queue"),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "merge_queue_rebounce".into(),
            before_commit_sha: Some("queue".into()),
        })
        .unwrap()
        .unwrap();
    db.connect().unwrap().execute("UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue', requeue_enqueued_at = 1 WHERE id = ?1", [&audit.attempt.id]).unwrap();
    // The revision itself is not blocked: re-arming it would be a no-op.
    assert!(!db.rearm_blocked_ci_failure_signal(&revision.id).unwrap());
    let publisher = crate::test_support::RecordingPublisher::default();
    assert!(rearm_and_notify(&db, &publisher, "exec", &audit).await);
    assert_eq!(
        *publisher.events.lock().await,
        vec![(product.id.clone(), parent.id.clone(), "queue_ci_audit_rearmed".into())]
    );
    assert!(!rearm_and_notify(&db, &publisher, "exec", &audit).await);
    assert_eq!(publisher.events.lock().await.len(), 1);
    let reasons: Vec<_> = db
        .active_blocked_signals(&parent.id)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect();
    assert_eq!(reasons, vec!["ci_failure"]);
    assert!(db.active_blocked_signals(&revision.id).unwrap().is_empty());
    let restored = db.get_ci_remediation(&audit.attempt.id).unwrap().unwrap();
    assert_eq!(restored.status, "pending");
    assert_eq!(restored.head_sha_at_trigger, format!("mq:{HEAD}"));
    let boundary: Option<i64> = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT requeue_enqueued_at FROM ci_remediations WHERE id = ?1",
            [&restored.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(boundary.is_none());
    assert!(restored.failure_reason.is_none());
    let candidate = crate::work::PendingMergeCheck {
        work_item_id: parent.id.clone(),
        product_id: product.id,
        pr_url: restored.pr_url,
    };
    let probe = boss_observation_contracts::PrLifecycleProbe::builder()
        .labels(vec![])
        .review(boss_observation_contracts::PrReviewState::Unknown)
        .url(&candidate.pr_url)
        .state(boss_observation_contracts::PrLifecycleState::Open(
            boss_observation_contracts::OpenPrStatus {
                mergeability: boss_observation_contracts::OpenPrMergeability::Clean,
                ci: boss_observation_contracts::OpenPrCiStatus::Clean,
            },
        ))
        .build();
    let mut outcome = crate::merge_poller::SweepOutcome::default();
    crate::merge_poller::maybe_clear_blocked(
        &db,
        &boss_execution_contracts::NoopExecutionPublisher,
        (None, None),
        &candidate,
        &probe,
        (&boss_observation_contracts::OpenPrCiStatus::Clean, true),
        &mut outcome,
    )
    .await;
    assert_eq!(
        db.task_blocked_reason(&parent.id).unwrap().as_deref(),
        Some("ci_failure")
    );
    // A sweep may have already cleared the block before the audit ran.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', blocked_reason = NULL WHERE id = ?1",
        [&parent.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue' WHERE id = ?1",
        [&audit.attempt.id],
    )
    .unwrap();
    drop(conn);
    assert!(rearm_parent_failure_signal(&db, "exec", &audit));
    assert_eq!(
        db.task_blocked_reason(&parent.id).unwrap().as_deref(),
        Some("ci_failure")
    );
    assert!(!rearm_parent_failure_signal(&db, "exec", &audit), "rearm is idempotent");
    // An explicit human suppression must still win over the audit.
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET status = 'in_review', blocked_reason = NULL WHERE id = ?1",
        [&parent.id],
    )
    .unwrap();
    conn.execute(
        "UPDATE ci_remediations SET status = 'abandoned', failure_reason = 'verified_requeue' WHERE id = ?1",
        [&audit.attempt.id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ci_failure_suppressions (work_item_id, head_sha, created_at) VALUES (?1, ?2, '0')",
        rusqlite::params![parent.id, HEAD],
    )
    .unwrap();
    drop(conn);
    assert!(!rearm_parent_failure_signal(&db, "exec", &audit));
    assert!(db.task_blocked_reason(&parent.id).unwrap().is_none());
}
