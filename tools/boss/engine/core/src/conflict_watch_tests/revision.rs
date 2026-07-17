use super::helpers::*;

// ----- Phase 3 cutover: engine-triggered revision as the fix vehicle -----

#[tokio::test]
async fn detection_spawns_revision_and_stamps_attempt() {
    // A genuinely-new conflict creates a `kind=revision` task (parent =
    // chore, merge-conflict provenance), stamps the ledger row's
    // `revision_task_id`, creates NO bespoke conflict_resolution execution,
    // and leaves the parent in `in_review` (new-model parent-state).
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/30";
    let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    assert!(
        on_conflict_detected(
            &db,
            pub_.as_ref(),
            None,
            &open_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
        )
        .await
    );

    // Parent stays in_review — the revision card is the Doing card.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(
        status,
        TaskStatus::InReview,
        "parent must stay in Review while revision is in flight"
    );
    assert!(reason.is_none());

    let attempt = db
        .active_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.status, "pending");
    let rev_id = attempt
        .revision_task_id
        .clone()
        .expect("the producer must stamp revision_task_id on the attempt");

    let revision = match db.get_work_item(&rev_id).unwrap() {
        WorkItem::Task(t) => t,
        other => panic!("expected revision task, got {other:?}"),
    };
    assert_eq!(revision.kind, TaskKind::Revision);
    assert_eq!(revision.parent_task_id.as_deref(), Some(chore.as_str()));
    assert_eq!(revision.created_via, format!("merge-conflict:{}", attempt.id));
    assert_eq!(revision.description, "Resolve merge conflict against main");

    // No bespoke conflict_resolution execution: the revision rides the
    // reconcile loop's revision_implementation dispatch instead.
    let ready = db.list_ready_executions().unwrap();
    assert!(
        !ready.iter().any(|e| e.kind == ExecutionKind::ConflictResolution),
        "cutover must not create a conflict_resolution execution; got {ready:?}",
    );
}

#[tokio::test]
async fn detection_idempotent_does_not_double_spawn_revision() {
    // Re-firing on the same base sha reuses the existing attempt (whose
    // revision_task_id is already set) and spawns no second revision.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/31";
    let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;
    // Reset to in_review so the second probe re-enters the primary flip
    // path with the same base sha (UNIQUE collision on the ledger).
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &open_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same base sha must not stack attempts");
    let revision_backed = attempts.iter().filter(|r| r.revision_task_id.is_some()).count();
    assert_eq!(revision_backed, 1, "exactly one revision-backed attempt");
}

#[tokio::test]
async fn create_revision_failure_abandons_attempt() {
    // When the create-time gate refuses (parent PR no longer open, R4),
    // the producer marks the ledger row `abandoned` so it never strands
    // as a pending attempt with no fix vehicle, and spawns no revision.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/33";
    let (product, chore) = make_in_review(&db, "C-rev-fail", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    let closed = crate::work::FakePrStateChecker::always(crate::work::PrOpenState::ClosedUnmerged);

    on_conflict_detected(
        &db,
        pub_.as_ref(),
        None,
        &closed,
        &candidate(&product, &chore, pr),
        &probe(pr, PrLifecycleState::Open(OpenPrStatus::conflict_only())),
    )
    .await;

    // The parent flip precedes the gate, so the chore is still blocked;
    // the poller's merged/closed handling reconciles it on a later sweep.
    let (status, reason) = chore_status(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let attempts = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].status, "abandoned");
    assert_eq!(attempts[0].failure_reason.as_deref(), Some("revision_create_failed"),);
    assert!(attempts[0].revision_task_id.is_none());
}
