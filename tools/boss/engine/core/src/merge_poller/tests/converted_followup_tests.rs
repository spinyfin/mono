use super::*;

#[tokio::test]
async fn merged_pr_redispatches_leaseless_review_revision_converted_in_place() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let pr = "https://github.com/foo/bar/pull/1502";
    let (product_id, parent_id) = make_chore_in_review(&db, "leaseless-review-followup", pr);
    let checker = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::Open);
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(parent_id.clone())
                .description("Address review findings after merge")
                .created_via(format!(
                    "{}exec_leaseless_review",
                    boss_protocol::CREATED_VIA_PR_REVIEW_PREFIX
                ))
                .build(),
            &checker,
        )
        .unwrap();
    let original = db.list_executions(Some(&revision.id)).unwrap().pop().unwrap();
    assert_eq!(original.kind, ExecutionKind::RevisionImplementation);
    assert_eq!(original.status, ExecutionStatus::Ready);
    assert!(original.cube_lease_id.is_none());

    // The revision has been activated, but its execution has not leased a
    // workspace yet. Only active revisions become autostart followups.
    db.connect()
        .unwrap()
        .execute("UPDATE tasks SET status = 'active' WHERE id = ?1", [&revision.id])
        .unwrap();
    db.mark_chore_pr_merged(&parent_id, pr).unwrap().unwrap();
    let followup = match db.get_work_item(&revision.id).unwrap() {
        WorkItem::Chore(task) => task,
        other => panic!("expected converted followup, got {other:?}"),
    };
    assert_eq!(followup.kind, TaskKind::Followup);
    assert_eq!(followup.status, TaskStatus::Todo);
    assert!(followup.autostart);
    assert_eq!(followup.product_id, product_id);
    assert_eq!(
        db.get_execution(&original.id).unwrap().status,
        ExecutionStatus::Abandoned
    );
    assert!(
        db.list_active_revision_executions_for_chain(&parent_id)
            .unwrap()
            .is_empty()
    );

    let publisher = Arc::new(RecordingPublisher::default());
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        Arc::new(FixedPrDetector(None)),
        Arc::new(NoopCubeClient),
        publisher.clone(),
        Arc::new(NoopPaneReleaser),
        Arc::new(NoopProbeQueuer),
    );
    let mut outcome = SweepOutcome::default();
    // Exercise the post-transaction merge step directly: no periodic
    // recovery sweep or invalidation publisher can mask an empty-loop bug.
    super::super::sweep::stop_active_revision_executions(
        &db,
        publisher.as_ref(),
        Some(&handler),
        &parent_id,
        &mut outcome,
    )
    .await;

    assert_eq!(outcome.revision_invalidated, 0, "there was no lease to clean up");
    let executions = db.list_executions(Some(&revision.id)).unwrap();
    assert_eq!(executions.len(), 2);
    let live: Vec<_> = executions
        .iter()
        .filter(|execution| !execution.status.is_terminal())
        .collect();
    assert_eq!(live.len(), 1, "converted row must receive exactly one live execution");
    assert_ne!(live[0].id, original.id);
    assert_eq!(live[0].kind, ExecutionKind::ChoreImplementation);
    assert_eq!(live[0].status, ExecutionStatus::Ready);
}
