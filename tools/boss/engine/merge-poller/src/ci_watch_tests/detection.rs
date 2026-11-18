use super::helpers::*;

#[tokio::test]
async fn detection_flips_in_review_to_blocked_ci_failure() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/10";
    let (product, chore) = make_in_review(&db, "C-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped, "first detection must flip the row");

    // In the in_review model a spawned revision immediately unblocks the
    // parent back to `in_review`; `blocked: ci_failure` is transient.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    let events = pub_.events.lock().await.clone();
    assert!(events.iter().any(|(_, _, r)| r == "ci_revision_in_flight"));

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiRemediationStarted { .. }))
    );

    // Counter incremented by one because we created a fix-kind attempt.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

#[tokio::test]
async fn detection_is_idempotent_on_repeated_probes() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/11";
    let (product, chore) = make_in_review(&db, "C-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let first = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let second = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(first);
    assert!(!second, "second probe with same head_sha must be a no-op");

    // Counter incremented exactly once across the duplicate probes.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

fn insert_conflict_attempt(
    db: &WorkDb,
    product: &str,
    chore: &str,
    pr: &str,
    pr_number: i64,
    base: &str,
    head: &str,
) -> crate::work::ConflictResolution {
    db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
        product_id: product.to_owned(),
        work_item_id: chore.to_owned(),
        pr_url: pr.into(),
        pr_number,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some(base.into()),
        head_sha_before: Some(head.into()),
    })
    .unwrap()
    .expect("fresh key must insert")
}

fn replant_pending(db_path: &std::path::Path, attempt_id: &str, base: &str) {
    rusqlite::Connection::open(db_path)
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions
                SET status = 'pending', failure_reason = NULL, finished_at = NULL,
                    base_sha_at_trigger = ?2
              WHERE id = ?1",
            rusqlite::params![attempt_id, base],
        )
        .unwrap();
}

#[tokio::test]
async fn detection_defers_when_live_conflict_resolution_exists() {
    // §Q7 composed ordering: a genuinely live conflict attempt (a worker
    // holds the lease) pre-empts the CI flow even on a mergeable PR.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12";
    let (product, chore) = make_in_review(&db, "C-defer-cr", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    let attempt = insert_conflict_attempt(&db, &product, &chore, pr, 12, "base-1", "head-1");
    db.mark_conflict_resolution_running(&attempt.id, "lease-1", "ws-1", "worker-1")
        .unwrap()
        .unwrap();
    // Reset to in_review so the WHERE guard would otherwise fire.
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "a running conflict-resolution must pre-empt CI flow");
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "row stays where it was");
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "no CI-fix may spawn while a conflict worker is live",
    );
}

#[tokio::test]
async fn stale_pending_conflict_on_mergeable_pr_does_not_suppress_ci_fix() {
    // Unblocked in-review parent, leftover `pending` conflict attempt from
    // a previous day, later attempt already succeeded, PR mergeable, CI
    // red. The leftover row must be retired and a CI-fix revision created.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/42";
    let (product, chore) = make_in_review(&db, "C-stale-pending-ci", pr);

    let stale = insert_conflict_attempt(&db, &product, &chore, pr, 42, "base-1", "head-old");
    let later = insert_conflict_attempt(&db, &product, &chore, pr, 42, "base-1", "head-new");
    db.mark_conflict_resolution_succeeded(&later.id, None).unwrap().unwrap();
    replant_pending(&db_path, &stale.id, "base-1");
    assert_eq!(
        db.active_conflict_resolution_for_work_item(&chore)
            .unwrap()
            .map(|a| a.id),
        Some(stale.id.clone()),
        "the leftover pending row is the active conflict attempt before CI-watch runs",
    );
    assert_eq!(chore_state(&db, &chore).0, TaskStatus::InReview);

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-fail"),
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "CI-watch must proceed once the leftover pending row is retired"
    );

    let stale_row = db.get_conflict_resolution(&stale.id).unwrap().unwrap();
    assert_eq!(stale_row.status, "abandoned");
    assert_eq!(
        stale_row.failure_reason.as_deref(),
        Some("superseded_by_newer_attempt"),
        "a leftover pending row next to a later succeeded attempt is superseded",
    );
    assert!(db.active_conflict_resolution_for_work_item(&chore).unwrap().is_none());

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a CI-fix attempt must be created");
    assert_eq!(attempt.attempt_kind, "fix");
    let rev_id = attempt
        .revision_task_id
        .clone()
        .expect("the producer must stamp a CI-fix revision");
    match db.get_work_item(&rev_id).unwrap() {
        WorkItem::Task(t) => {
            assert_eq!(t.kind, TaskKind::Revision);
            assert_eq!(t.parent_task_id.as_deref(), Some(chore.as_str()));
        }
        other => panic!("expected revision task, got {other:?}"),
    }
}

#[tokio::test]
async fn detection_defers_when_active_rebase_attempt_exists() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/13";
    let (product, chore) = make_in_review(&db, "C-defer-rebase", pr);
    // Stand up the auto-rebase side table directly so the deferral
    // gate observes a non-terminal row.
    let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
    conn.execute(
        "CREATE TABLE rebase_attempts (
             id                TEXT PRIMARY KEY,
             dependent_pr_url  TEXT NOT NULL,
             status            TEXT NOT NULL
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
          VALUES ('reb_1', ?1, 'running')",
        [pr],
    )
    .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "active rebase attempt must pre-empt CI flow");
}

fn insert_rebase_row(db_path: &std::path::Path, id: &str, pr: &str, status: &str) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS rebase_attempts (
             id                TEXT PRIMARY KEY,
             dependent_pr_url  TEXT NOT NULL,
             status            TEXT NOT NULL
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO rebase_attempts (id, dependent_pr_url, status)
          VALUES (?1, ?2, ?3)",
        rusqlite::params![id, pr, status],
    )
    .unwrap();
}

#[tokio::test]
async fn pending_rebase_on_clean_pr_does_not_preempt_ci() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/43";
    let (product, chore) = make_in_review(&db, "C-pending-rebase-clean", pr);
    insert_rebase_row(&db_path, "reb_pending", pr, "pending");

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "a leftover pending rebase row on a mergeable PR must not pre-empt CI"
    );
}

#[tokio::test]
async fn pending_conflict_on_unknown_mergeability_still_defers() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/44";
    let (product, chore) = make_in_review(&db, "C-pending-unknown", pr);
    let attempt = insert_conflict_attempt(&db, &product, &chore, pr, 44, "base-1", "head-1");

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe_unknown_mergeability(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(
        !flipped,
        "a pending conflict row still pre-empts CI while mergeability is unknown"
    );
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "pending",
        "a bare pending row is not stale while mergeability is unknown"
    );
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

fn attach_dead_conflict_revision(db: &WorkDb, chore: &str, attempt_id: &str) -> String {
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.to_owned())
                .description("Resolve merge conflict against main")
                .created_via(format!("merge-conflict:{attempt_id}"))
                .build(),
            &fix_checker(),
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(attempt_id, &revision.id)
        .unwrap()
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET status = 'in_review' WHERE id = ?1",
            rusqlite::params![revision.id],
        )
        .unwrap();
    revision.id
}

#[tokio::test]
async fn superseded_pending_conflict_on_unknown_mergeability_does_not_suppress_ci_fix() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/45";
    let (product, chore) = make_in_review(&db, "C-superseded-unknown", pr);

    let stale = insert_conflict_attempt(&db, &product, &chore, pr, 45, "base-1", "head-old");
    let later = insert_conflict_attempt(&db, &product, &chore, pr, 45, "base-1", "head-new");
    db.mark_conflict_resolution_succeeded(&later.id, None).unwrap().unwrap();
    replant_pending(&db_path, &stale.id, "base-1");

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe_unknown_mergeability(pr, "head-fail"),
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "a superseded pending row must be retired even while mergeability is unknown"
    );
    let stale_row = db.get_conflict_resolution(&stale.id).unwrap().unwrap();
    assert_eq!(stale_row.status, "abandoned");
    assert_eq!(stale_row.failure_reason.as_deref(), Some("superseded_by_newer_attempt"));
}

#[tokio::test]
async fn superseded_pending_with_dead_revision_on_unknown_mergeability_does_not_suppress_ci_fix() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/49";
    let (product, chore) = make_in_review(&db, "C-dead-rev-unknown", pr);

    let stale = insert_conflict_attempt(&db, &product, &chore, pr, 49, "base-1", "head-old");
    attach_dead_conflict_revision(&db, &chore, &stale.id);
    let later = insert_conflict_attempt(&db, &product, &chore, pr, 49, "base-1", "head-new");
    db.mark_conflict_resolution_succeeded(&later.id, None).unwrap().unwrap();
    assert_eq!(
        db.get_conflict_resolution(&stale.id).unwrap().unwrap().status,
        "pending",
        "a dead-revision pending row survives insert of a later attempt"
    );

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe_unknown_mergeability(pr, "head-fail"),
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "a superseded pending row with a dead revision must be retired even while mergeability is unknown"
    );
    let stale_row = db.get_conflict_resolution(&stale.id).unwrap().unwrap();
    assert_eq!(stale_row.status, "abandoned");
    assert_eq!(stale_row.failure_reason.as_deref(), Some("superseded_by_newer_attempt"));
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a CI-fix attempt must be created");
    assert_eq!(attempt.attempt_kind, "fix");
}

#[tokio::test]
async fn older_mechanical_rung_preempts_ci_when_newer_pending_revision_is_dead() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/50";
    let (product, chore) = make_in_review(&db, "C-older-mech-dead-rev", pr);

    let live = insert_conflict_attempt(&db, &product, &chore, pr, 50, "base-1", "head-old");
    db.stamp_conflict_resolution_mechanical_rung(&live.id, 1, "lease-1", "ws-1")
        .unwrap()
        .unwrap();
    let newer = insert_conflict_attempt(&db, &product, &chore, pr, 50, "base-1", "head-new");
    attach_dead_conflict_revision(&db, &chore, &newer.id);
    assert_eq!(
        db.active_conflict_resolution_for_work_item(&chore)
            .unwrap()
            .map(|a| a.id),
        Some(newer.id.clone()),
        "the newest pending row is the dead-revision attempt"
    );

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-fail"),
        &one_failure(),
    )
    .await;
    assert!(
        !flipped,
        "an older in-flight mechanical rung must still pre-empt CI on a mergeable PR"
    );
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
    let live_row = db.get_conflict_resolution(&live.id).unwrap().unwrap();
    assert_eq!(live_row.status, "pending");
    assert_eq!(live_row.mechanical_rung_in_flight, Some(1));
    let newer_row = db.get_conflict_resolution(&newer.id).unwrap().unwrap();
    assert_eq!(newer_row.status, "abandoned");
    assert_eq!(newer_row.failure_reason.as_deref(), Some("pr_mergeable_attempt_stale"));
}

#[tokio::test]
async fn mechanical_rung_conflict_preempts_ci_on_mergeable_pr() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/46";
    let (product, chore) = make_in_review(&db, "C-mech-rung", pr);
    let attempt = insert_conflict_attempt(&db, &product, &chore, pr, 46, "base-1", "head-1");
    db.stamp_conflict_resolution_mechanical_rung(&attempt.id, 1, "lease-1", "ws-1")
        .unwrap()
        .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "an in-flight mechanical rung is a live conflict writer");
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "pending"
    );
}

#[tokio::test]
async fn pending_conflict_with_queued_revision_preempts_ci() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/47";
    let (product, chore) = make_in_review(&db, "C-queued-rev", pr);
    let attempt = insert_conflict_attempt(&db, &product, &chore, pr, 47, "base-1", "head-1");
    let revision = db
        .create_revision(
            boss_protocol::CreateRevisionInput::builder()
                .parent_task_id(chore.clone())
                .description("Resolve merge conflict against main")
                .created_via(format!("merge-conflict:{}", attempt.id))
                .build(),
            &fix_checker(),
        )
        .unwrap();
    db.set_conflict_resolution_revision_task_id(&attempt.id, &revision.id)
        .unwrap()
        .unwrap();
    db.create_execution(
        crate::work::CreateExecutionInput::builder()
            .work_item_id(revision.id.clone())
            .kind(crate::work::ExecutionKind::RevisionImplementation)
            .status(crate::work::ExecutionStatus::Ready)
            .build(),
    )
    .unwrap();

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "a pending conflict attempt with a queued revision is live");
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

#[tokio::test]
async fn in_flight_ci_fix_revision_logs_blocking_attempt_id() {
    let logs = log_capture::install();
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/48";
    let (product, chore) = make_in_review(&db, "C-inflight-log", pr);
    let pub_ = Arc::new(RecordingPublisher::default());
    assert!(
        on_ci_failure_detected(
            &db,
            pub_.as_ref(),
            &fix_checker(),
            &candidate(&product, &chore, pr),
            &probe(pr, "head-1"),
            &one_failure(),
        )
        .await
    );
    let attempt = db.active_ci_remediation_for_work_item(&chore).unwrap().unwrap();
    let revision_id = attempt.revision_task_id.clone().expect("revision");
    db.mark_ci_remediation_succeeded(&attempt.id, Some("head-1"))
        .unwrap()
        .unwrap();
    assert!(
        db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "the CI-fix revision is still a live writer after the ledger row succeeds"
    );

    logs.lock().clear();
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-2"),
        &one_failure(),
    )
    .await;
    assert!(!flipped, "an in-flight CI-fix revision must defer a new attempt");
    let text = String::from_utf8(logs.lock().clone()).unwrap();
    assert!(
        text.contains("gate=\"in_flight_ci_fix_revision\"") || text.contains("in_flight_ci_fix_revision"),
        "deferral must name the gate: {text}"
    );
    assert!(
        text.contains(&attempt.id),
        "deferral must name the blocking remediation attempt: {text}"
    );
    assert!(
        text.contains(&revision_id),
        "deferral must name the blocking revision: {text}"
    );
}

#[tokio::test]
async fn in_flight_ci_fix_revision_logs_when_no_ledger_row() {
    let logs = log_capture::install();
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/49";
    let (product, chore) = make_in_review(&db, "C-inflight-no-ledger", pr);
    let conn = db.connect().unwrap();
    let revision_id = "task_cifix_no_ledger";
    conn.execute(
        "INSERT INTO tasks (id, product_id, kind, name, description, status, created_at, updated_at, autostart, created_via, parent_task_id)
         VALUES (?1, ?2, 'revision', 'Fix failing CI on PR', '', 'active', '1700000000', '1700000000', 0, ?3, ?4)",
        rusqlite::params![
            revision_id,
            product,
            format!("{}rem_missing", boss_protocol::CREATED_VIA_CI_FIX_PREFIX),
            chore
        ],
    )
    .unwrap();
    drop(conn);

    logs.lock().clear();
    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(!flipped);
    let text = String::from_utf8(logs.lock().clone()).unwrap();
    assert!(
        text.contains("in_flight_ci_fix_revision"),
        "deferral must name the gate: {text}"
    );
    assert!(
        text.contains(revision_id),
        "deferral must name the blocking revision: {text}"
    );
    assert!(
        text.contains("ledger_row") && text.contains("none"),
        "deferral must state that no ledger row exists: {text}"
    );
}

#[tokio::test]
async fn detection_lands_exhausted_when_budget_is_zero() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/14";
    let (product, chore) = make_in_review(&db, "C-exh", pr);
    // Set the per-product budget to 0 ("notify only").
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute("UPDATE products SET ci_attempt_budget = 0 WHERE id = ?1", [&product])
        .unwrap();
    drop(conn);

    let pub_ = Arc::new(RecordingPublisher::default());
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped);
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure_exhausted"));

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiRemediationExhausted { .. }))
    );
    // No attempt row should have been inserted.
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());
}

#[tokio::test]
async fn detection_skipped_when_pr_has_opt_out_label() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/15";
    let (product, chore) = make_in_review(&db, "C-optout", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe_with_labels(pr, "head-1", &["boss/no-auto-rebase"]),
        &one_failure(),
    )
    .await;
    assert!(!flipped);
}

#[tokio::test]
async fn detection_requires_head_ref_oid() {
    // Without `headRefOid` the engine can't key the attempt row,
    // so we leave the parent alone and wait for the next probe.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/16";
    let (product, chore) = make_in_review(&db, "C-no-head", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let mut p = probe(pr, "head-1");
    p.head_ref_oid = None;
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &p,
        &one_failure(),
    )
    .await;
    assert!(!flipped);
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
}

#[tokio::test]
async fn full_cycle_detect_then_retire() {
    // Probe → attempt → push (simulated) → next probe Clean → retire.
    // Idempotency: a second Clean probe is a no-op.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/17";
    let (product, chore) = make_in_review(&db, "C-cycle", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect.
    let detected = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(detected);
    // In the in_review model the parent stays in_review while the revision runs.
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    let ci_fix_revision_id = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .and_then(|a| a.revision_task_id)
        .expect("detection must spawn a CI-fix revision");

    // 2. Retire — CI is back to clean.
    let resolved = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(resolved);
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Mode A/B parity fix: CI resolving must close the revision it spawned,
    // mirroring conflict_watch::on_resolved — otherwise it sits open on the
    // board (parked at `in_review`, not tombstoned) until the parent PR
    // eventually merges.
    let closed_revision = task(&db, &ci_fix_revision_id);
    assert_eq!(
        closed_revision.status,
        TaskStatus::Archived,
        "the resolved CI-fix revision must be archived, not merely settled to in_review"
    );
    assert!(
        closed_revision.deleted_at.is_some(),
        "the resolved CI-fix revision must be tombstoned so it disappears from the board"
    );
    assert_eq!(closed_revision.archived_by.as_deref(), Some("ci_watch_supersession"));
    assert!(closed_revision.archived_at.as_deref().is_some_and(|at| !at.is_empty()));
    assert!(
        closed_revision
            .archived_reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty())
    );
    assert_eq!(closed_revision.last_status_actor, "engine");

    // Attempt row terminal.
    let attempts: Vec<_> = {
        let conn = rusqlite::Connection::open(dir.path().join("boss.db")).unwrap();
        let mut stmt = conn
            .prepare("SELECT status FROM ci_remediations WHERE work_item_id = ?1")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([&chore], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    };
    assert_eq!(attempts, vec!["succeeded".to_owned()]);

    // 3. Counter reset on successful cycle.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

    // 4. Repeat retire — no-op.
    let again = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(!again);
}

#[tokio::test]
async fn retire_skipped_when_product_opt_out_flag_disabled() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/18";
    let (product, chore) = make_in_review(&db, "C-optout-retire", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Detect first so there's something to retire.
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        "UPDATE products SET auto_pr_maintenance_enabled = 0 WHERE id = ?1",
        [&product],
    )
    .unwrap();
    drop(conn);

    let retired = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(!retired, "opted-out product must not retire automatically");
    // In the in_review model the parent was never blocked; the retire
    // no-op leaves it in_review.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

/// When `on_ci_resolved` clears a `blocked: ci_failure` row but finds
/// no active (pending/running) remediation attempt — because the prior
/// attempt was already terminal (failed, abandoned) — it must emit
/// `CiFailureCleared` so the UI can clear its stale `ci failing` badge
/// without incorrectly setting the `ci auto-fixed` badge.
#[tokio::test]
async fn retire_without_active_attempt_emits_ci_failure_cleared() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/19";
    let (product, chore) = make_in_review(&db, "C-no-active-attempt", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect failure → attempt created and marked failed (simulating
    //    a worker that ran but couldn't push a fix).
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");
    db.mark_ci_remediation_failed(&attempt.id, "no_push_no_classification")
        .unwrap();

    // 2. CI goes green on its own — no active attempt left.
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "attempt must be terminal before retire"
    );
    let resolved = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(resolved, "retire must succeed even without active attempt");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    // Engine must emit CiFailureCleared (not CiRemediationSucceeded)
    // so the UI clears the failure badge without setting auto-fixed.
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
        )),
        "CiFailureCleared must be emitted when task clears without active attempt"
    );
    assert!(
        !typed
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiRemediationSucceeded { .. })),
        "CiRemediationSucceeded must NOT be emitted when there is no active attempt"
    );
}

/// Issue #901: a chore left in `blocked: ci_failure` from a prior
/// run is superseded once CI re-enters InFlight (no active
/// remediation). The chore returns to `in_review`, `CiFailureCleared`
/// is emitted so the UI drops the stale badge, and the CI budget
/// counter is preserved (the run hasn't passed yet).
#[tokio::test]
async fn in_flight_supersedes_stale_ci_failure() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/901";
    let (product, chore) = make_in_review(&db, "C-supersede", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect failure → blocked: ci_failure, budget=1, attempt
    //    created. Then mark the attempt failed so no active
    //    remediation remains (a worker that ran but couldn't push).
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");
    db.mark_ci_remediation_failed(&attempt.id, "no_push_no_classification")
        .unwrap();
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // 2. CI re-runs (InFlight) — the stale failure is superseded.
    // The attempt was marked failed, so active_ci_remediation returns None;
    // any head SHA passes.
    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-1"),
    )
    .await;
    assert!(cleared, "stale ci_failure must be superseded by InFlight");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
        )),
        "CiFailureCleared must drop the stale badge",
    );
    let events = pub_.events.lock().await.clone();
    assert!(events.iter().any(|(_, _, r)| r == "ci_failure_superseded_in_progress"),);

    // Budget is NOT reset — the re-run hasn't passed yet, so a fresh
    // failure must keep consuming the remaining allotment.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
}

/// An *active* remediation attempt owns the slot: its own fix push is
/// what re-triggered CI, so its in-flight chip must not be cleared.
/// The supersede path declines and the chore stays blocked.
#[tokio::test]
async fn in_flight_supersede_skips_when_active_remediation() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/902";
    let (product, chore) = make_in_review(&db, "C-active-rem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Detection leaves a pending (active) remediation attempt.
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_some(),
        "attempt must be active before the supersede check",
    );

    // Same head SHA as the active remediation → the fix worker's own re-run; must not supersede.
    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-1"),
    )
    .await;
    assert!(!cleared, "active remediation for same head must not be superseded");

    // In the in_review model the parent stays in_review while the revision runs.
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());
}

/// No stale failure to supersede (chore already `in_review`): the
/// supersede path is a cheap WHERE-guard no-op and emits nothing.
#[tokio::test]
async fn in_flight_supersede_noop_when_in_review() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/903";
    let (product, chore) = make_in_review(&db, "C-noop", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    let cleared =
        on_ci_in_flight_supersedes_failure(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[], None).await;
    assert!(!cleared, "an in_review chore has no stale failure to clear");

    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(pub_.typed_events.lock().await.is_empty());
    assert!(pub_.events.lock().await.is_empty());
}

/// The opt-out label suppresses the supersede just like the detect /
/// retire paths: a stale ci_failure on an opted-out PR is left alone.
#[tokio::test]
async fn in_flight_supersede_skipped_when_pr_has_opt_out_label() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/904";
    let (product, chore) = make_in_review(&db, "C-supersede-optout", pr);
    db.mark_chore_blocked_ci_failure(&chore, pr, None).unwrap();
    let pub_ = Arc::new(RecordingPublisher::default());

    let cleared = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &["boss/no-auto-rebase".to_owned()],
        Some("head-1"),
    )
    .await;
    assert!(!cleared, "opt-out label must suppress the supersede");

    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(reason.as_deref(), Some("ci_failure"));
}

/// First InFlight probe records `first_observed_at` but emits
/// nothing (no threshold crossed). A subsequent probe whose
/// observed timestamp is rewound by >30min lands in the `warn`
/// bucket; rewinding past 2h lands in `alert`. Repeated probes at
/// the same bucket are no-ops (the WHERE guard rejects same-level
/// re-emits).
#[tokio::test]
async fn never_starts_alert_crosses_warn_then_alert() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/30";
    let (product, chore) = make_in_review(&db, "C-never-starts", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Probe #1: no threshold crossed.
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "none");
    let typed_after_first = pub_.typed_events.lock().await.clone();
    assert!(typed_after_first.is_empty(), "no event before any bucket");

    // Rewind the observation timestamp by 31 min so the next probe
    // crosses the warn threshold.
    let warn_cutoff = current_unix_secs() - (31 * 60);
    rewind_inflight_observation(&db_path, &chore, "head-A", warn_cutoff);
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "warn");
    // Still no soft-alert frontend event — warn is log-only.
    let typed_after_warn = pub_.typed_events.lock().await.clone();
    assert!(
        typed_after_warn
            .iter()
            .all(|(_, ev)| !matches!(ev, FrontendEvent::CiNeverStartsAlert { .. })),
        "warn bucket must not emit CiNeverStartsAlert event",
    );

    // A second probe at the same elapsed bucket is a no-op (the
    // alert-level WHERE guard rejects a same-level rewrite).
    let again = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(again, "warn");

    // Rewind past 2h so the next probe upgrades to alert.
    let alert_cutoff = current_unix_secs() - (2 * 60 * 60 + 60);
    rewind_inflight_observation(&db_path, &chore, "head-A", alert_cutoff);
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "alert");
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiNeverStartsAlert {
                level,
                ..
            } if level == "2h"
        )),
        "alert bucket must emit CiNeverStartsAlert with level=2h",
    );
}

/// A fresh push (new head sha) keys observations on its own row,
/// so the timer restarts from zero and the previous bucket doesn't
/// carry over.
#[tokio::test]
async fn never_starts_alert_resets_on_new_head_sha() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/31";
    let (product, chore) = make_in_review(&db, "C-new-head", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Drive head-A all the way to `alert`.
    on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    rewind_inflight_observation(&db_path, &chore, "head-A", current_unix_secs() - (3 * 60 * 60));
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
    )
    .await;
    assert_eq!(level, "alert");

    // A new head sha starts fresh.
    let level = on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-B"),
    )
    .await;
    assert_eq!(level, "none", "new head sha must reset the timer");
}

/// When the engine flips the chore to `blocked: ci_failure` (CI
/// transitions from InFlight to Failing), the leftover observation
/// row must be cleared so a later InFlight stretch starts fresh.
#[tokio::test]
async fn detection_clears_inflight_observation() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/32";
    let (product, chore) = make_in_review(&db, "C-clear-on-detect", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_ci_in_flight(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
    )
    .await;
    let n: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM ci_inflight_observations WHERE work_item_id = ?1",
            [&chore],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "observation row exists after InFlight probe");

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    let n: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM ci_inflight_observations WHERE work_item_id = ?1",
            [&chore],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "Failing detection must clear inflight observations");
}

fn current_unix_secs() -> i64 {
    boss_engine_utils::epoch_time::now_epoch_secs()
}

/// Rewrite the `first_observed_at` timestamp on a
/// `ci_inflight_observations` row to simulate the passage of time
/// without sleeping. Used by the never-starts-alert tests.
fn rewind_inflight_observation(db_path: &std::path::Path, work_item_id: &str, head_sha: &str, when_unix_secs: i64) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "UPDATE ci_inflight_observations
            SET first_observed_at = ?3
          WHERE work_item_id = ?1 AND head_sha = ?2",
        rusqlite::params![work_item_id, head_sha, when_unix_secs.to_string()],
    )
    .unwrap();
}

/// Stale-badge scenario: a push to the PR changes the head SHA while the prior run's `ci_remediations`
/// row is still `pending`. The new CI run is all-in-flight (no failing leaf).
///
/// `on_ci_in_flight_supersedes_failure` must not bail on that pending row (it is
/// for the old head SHA): the stale row is abandoned and
/// `CiFailureCleared` is emitted so the badge correctly reflects the new run.
#[tokio::test]
async fn new_commit_all_inflight_abandons_stale_remediation_and_clears_badge() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1160";
    let (product, chore) = make_in_review(&db, "C-stale-badge", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // --- Step 1: Prior commit (head-A) terminally fails CI and a remediation is created. ---
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
        &one_failure(),
    )
    .await;

    let prior_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending remediation row must exist after detection");
    assert_eq!(
        prior_attempt.head_sha_at_trigger, "head-A",
        "remediation row must record the head SHA at trigger",
    );
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // --- Step 2: User pushes a new commit (head-B). GitHub restarts CI
    // from scratch — all checks are now queued / running (InFlight).
    // NO failing leaf in this new rollup: this is the all-in-progress case.
    // The prior remediation row is still `pending` (the fix worker hasn't
    // done anything yet — the push made its revision moot). ---
    pub_.events.lock().await.clear();
    pub_.typed_events.lock().await.clear();

    let superseded = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-B"), // new head SHA — DIFFERENT from the pending row's head_sha_at_trigger
    )
    .await;

    assert!(
        superseded,
        "InFlight at a new head SHA must supersede the stale remediation and clear the badge",
    );

    // The stale row must be abandoned — not terminal-failed, not pending.
    let still_active = db.active_ci_remediation_for_work_item(&chore).unwrap();
    assert!(
        still_active.is_none(),
        "the stale remediation row must be abandoned, not left pending",
    );

    let retired = db.get_ci_remediation(&prior_attempt.id).unwrap().unwrap();
    assert_eq!(retired.failure_reason.as_deref(), Some("new_head_sha_inflight"));
    assert_eq!(retired.head_sha_after.as_deref(), Some("head-B"));

    // `CiFailureCleared` must be emitted so the macOS badge clears.
    let typed = pub_.typed_events.lock().await.clone();
    assert!(
        typed.iter().any(|(_, ev)| matches!(
            ev,
            FrontendEvent::CiFailureCleared { pr_url, .. } if pr_url == pr
        )),
        "CiFailureCleared must be emitted when stale remediation is superseded by a new head",
    );

    // Budget counter is NOT reset — the new run hasn't passed yet.
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        1,
        "budget counter must not reset until CI actually passes",
    );

    // --- Step 3: same-head-SHA guard still holds — a fix worker's own CI re-run
    // at the SAME head SHA must NOT be superseded (or the badge would vanish while
    // the fix is running). Create a fresh remediation for head-C and then probe
    // InFlight at head-C — should NOT supersede. ---
    pub_.events.lock().await.clear();
    pub_.typed_events.lock().await.clear();

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-C"),
        &one_failure(),
    )
    .await;

    let not_superseded = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-C"), // SAME head SHA as the active remediation
    )
    .await;

    assert!(
        !not_superseded,
        "active remediation for the same head SHA must NOT be superseded (fix worker's own run)",
    );

    let typed_after = pub_.typed_events.lock().await.clone();
    assert!(
        !typed_after
            .iter()
            .any(|(_, ev)| matches!(ev, FrontendEvent::CiFailureCleared { .. })),
        "CiFailureCleared must NOT be emitted when the active remediation is for the same head",
    );
}

/// Mode A regression ("merge-conflict/CI-fix rows should be tombstoned
/// automatically once their underlying PR merges"): superseding a stale
/// ci_remediations attempt at the attempt-table level used to leave the
/// CI-fix revision it had already spawned open on the board forever —
/// nothing else closes it until the parent PR eventually merges. A fresh
/// commit landing must close the stale row's revision, not just abandon
/// the attempt.
#[tokio::test]
async fn in_flight_supersede_closes_the_stale_remediations_revision() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/1161";
    let (product, chore) = make_in_review(&db, "C-stale-revision", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-A"),
        &one_failure(),
    )
    .await;

    let prior_attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending remediation row must exist after detection");
    let prior_revision_id = prior_attempt
        .revision_task_id
        .clone()
        .expect("detection must spawn a CI-fix revision");

    let superseded = on_ci_in_flight_supersedes_failure(
        &db,
        pub_.as_ref(),
        &candidate(&product, &chore, pr),
        &[],
        Some("head-B"), // new head SHA — DIFFERENT from the pending row's head_sha_at_trigger
    )
    .await;
    assert!(
        superseded,
        "InFlight at a new head SHA must supersede the stale remediation"
    );

    let closed_revision = task(&db, &prior_revision_id);
    assert_eq!(
        closed_revision.status,
        TaskStatus::Archived,
        "the superseded CI-fix revision must be archived, not left open"
    );
    assert!(
        closed_revision.deleted_at.is_some(),
        "the superseded CI-fix revision must be tombstoned so it disappears from the board"
    );
}

#[test]
fn encode_failed_checks_round_trip() {
    let json = encode_failed_checks(&[RequiredCheckFailure {
        job_key: None,
        name: "ci/test".into(),
        conclusion: "FAILURE".into(),
        target_url: "https://github.com/foo/bar/actions/runs/1/job/2".into(),
        provider: CiProvider::GithubActions,
        provider_job_id: Some("2".into()),
    }]);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let item = &arr[0];
    assert_eq!(item["name"], "ci/test");
    assert_eq!(item["provider"], "github_actions");
    assert_eq!(item["provider_job_id"], "2");
}
