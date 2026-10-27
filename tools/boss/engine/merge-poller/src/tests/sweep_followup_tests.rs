use super::*;

/// A PR-review revision whose target merges while its worker is live must
/// cross execution families automatically: cancel the obsolete
/// revision_implementation, preserve the work item as an autostart followup,
/// and enqueue a chore_implementation without any manual start.
/// The cancellation must also close the original dispatch timeline.
#[tokio::test]
async fn merged_pr_redispatches_live_review_revision_as_followup_and_records_cancel() {
    let dir = tempdir().unwrap();
    let events = Arc::new(RecordingDispatchEventSink::new());
    let db = Arc::new(
        WorkDb::open(dir.path().join("boss.db"))
            .unwrap()
            .with_dispatch_events(events.clone()),
    );
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

    let publisher = Arc::new(RecordingPublisher::default());
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        publisher.clone(),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
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
        None,
    )
    .await;

    assert_eq!(outcome.merged, 1);
    assert_eq!(outcome.revision_invalidated, 1);
    let cancelled = db.get_execution(&original.id).unwrap();
    assert_cancel_provenance(
        &cancelled,
        boss_protocol::CancelSource::MergePollerParentMerged,
        "merge_poller",
        "merged, so this revision execution is obsolete",
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
    assert_eq!(cancellation.details["prior_status"], "running");
    assert_eq!(cancellation.details["chain_root_id"], parent_id);
    assert!(
        cancellation.details["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("parent PR of chain root"),
        "dispatch cancel reason must be the merge-poller cause, got {:?}",
        cancellation.details["reason"]
    );
    assert!(
        crate::dispatch_reader::is_terminal_event(&cancellation),
        "merge cancellation must close the original dispatch timeline",
    );
}
