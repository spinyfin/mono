use super::helpers::*;

// ----- Phase 4 cutover: engine-triggered revision as the fix vehicle -----

#[tokio::test]
async fn detection_spawns_revision_and_stamps_attempt() {
    // A genuinely-new `fix`-kind CI failure creates a `kind=revision`
    // task (parent = chore, ci-fix provenance), stamps the ledger row's
    // `revision_task_id`, and creates NO bespoke ci_remediation
    // execution — the dormant path stays dormant and the row is hidden
    // from the rescue recovery query.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/100";
    let (product, chore) = make_in_review(&db, "C-rev-spawn", pr);
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

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.status, "pending");
    assert_eq!(attempt.attempt_kind, "fix");
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
    assert_eq!(revision.created_via, format!("ci-fix:{}", attempt.id));
    assert_eq!(revision.description, "Fix failing CI: ci/test");

    // No bespoke ci_remediation execution: the revision rides the
    // reconcile loop's revision_implementation dispatch instead.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND kind = ?2",
            rusqlite::params![&chore, "ci_remediation"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0, "cutover must not create a ci_remediation execution");

    // The revision-backed row is invisible to the dormant rescue path.
    assert!(
        db.list_stranded_ci_remediation_attempts().unwrap().is_empty(),
        "revision-backed attempt must be excluded from the rescue query",
    );
}

#[tokio::test]
async fn detection_idempotent_does_not_double_spawn_revision() {
    // Re-firing on the same head sha reuses the existing attempt (whose
    // revision_task_id is already set) and spawns no second revision.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/101";
    let (product, chore) = make_in_review(&db, "C-rev-idem", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    // Reset to in_review so the second probe re-enters the primary flip
    // path with the same head sha (UNIQUE collision on the ledger).
    db.update_work_item(
        &chore,
        WorkItemPatch {
            status: Some("in_review".into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;

    let attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert_eq!(attempts.len(), 1, "same head sha must not stack attempts");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 1, "same head sha must not stack revisions");
}

#[tokio::test]
async fn retrigger_creates_bespoke_execution_and_no_revision() {
    // `retrigger` produces no commit, so it stays on the bespoke
    // ci_remediation execution kind (design Q6) and never spawns a
    // revision or consumes budget.
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/102";
    let (product, chore) = make_in_review(&db, "C-retrigger", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // All-infra failures classify as `retrigger`.
    let infra = vec![failure("ci/flaky", "STARTUP_FAILURE")];
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &infra,
    )
    .await;
    assert!(flipped);

    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("a pending attempt row must exist");
    assert_eq!(attempt.attempt_kind, "retrigger");
    assert!(
        attempt.revision_task_id.is_none(),
        "retrigger must not spawn a revision",
    );

    // Exactly one bespoke ci_remediation execution; no revision task.
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let exec_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM work_executions WHERE work_item_id = ?1 AND kind = 'ci_remediation'",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exec_count, 1, "retrigger must park a ci_remediation execution");
    let revisions: i64 = conn
        .query_row("SELECT COUNT(*) FROM tasks WHERE kind = 'revision'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(revisions, 0, "retrigger must not create a revision");

    // Retrigger does not consume the fix budget.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
}

#[tokio::test]
/// Regression guard for PR #1404: a new CI-fix revision must
/// not spawn while a prior attempt's revision worker is still in flight (status
/// `todo` or `active`), even when the `ci_remediations` row was prematurely
/// retired by `ci_attempt_signal_cleared` (the originally-failing checks are
/// no longer in the failing set after a flaky re-trigger, while the worker has
/// not pushed a fix commit yet).
async fn detection_defers_when_prior_ci_fix_revision_still_in_flight() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/200";
    let (product, chore) = make_in_review(&db, "C-overlap-guard", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: first CI failure on head-1 → attempt A, revision R1.
    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;
    assert!(flipped, "first detection must transition the chore");

    let attempt_a = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt A must exist");
    let rev_id = attempt_a
        .revision_task_id
        .clone()
        .expect("revision_task_id must be stamped");

    // Step 2: simulate premature retirement of attempt A — the originally-
    // failing checks are no longer in the failure set (e.g. a re-triggered
    // flaky check now passes while R1's worker is still running with no push).
    db.mark_ci_remediation_succeeded(&attempt_a.id, None).unwrap();

    // Verify R1 is still `todo` (worker has not started yet / no push).
    let rev_task = match db.get_work_item(&rev_id).unwrap() {
        crate::work::WorkItem::Task(t) => t,
        other => panic!("expected task, got {other:?}"),
    };
    assert_eq!(
        rev_task.status,
        TaskStatus::Todo,
        "R1 must still be todo — worker has not pushed",
    );

    // Verify primary gate is now bypassed (no active ci_remediations row).
    assert!(
        db.active_ci_remediation_for_work_item(&chore).unwrap().is_none(),
        "primary gate bypassed: no active ci_remediations row",
    );

    // Step 3: chore moves back to in_review (as it would be after
    // unblock_for_revision on a CI that appears clean momentarily).
    db.update_work_item(
        &chore,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();

    // Step 4: CI is still failing (perhaps different checks). Without the
    // secondary pre-flight guard this would spawn a second revision while R1
    // is still in flight. With the guard, spawning must be deferred.
    let flipped2 = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"), // same head — same failing SHA
        &one_failure(),
    )
    .await;
    assert!(
        !flipped2,
        "second detection must be deferred while R1 is still in flight (todo)",
    );

    // Only one ci_remediations row and one revision must exist.
    let all_attempts = db.list_ci_remediations(None, &[], Some(&chore), None).unwrap();
    assert_eq!(all_attempts.len(), 1, "must not create a second attempt row");

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 1, "must not spawn a second revision while R1 is active");
}

#[tokio::test]
/// After R1 pushes (moves to `in_review`) and CI fails on the pushed commit,
/// a new attempt IS allowed — the previous worker completed its job.
async fn detection_allowed_after_prior_revision_pushes() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("boss.db");
    let db = WorkDb::open(db_path.clone()).unwrap();
    let pr = "https://github.com/foo/bar/pull/201";
    let (product, chore) = make_in_review(&db, "C-overlap-after-push", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // Step 1: first CI failure → attempt A, revision R1.
    on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-1"),
        &one_failure(),
    )
    .await;

    let attempt_a = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt A");
    let rev_id = attempt_a.revision_task_id.clone().expect("revision_task_id");

    // Step 2: R1 pushes a commit (moves to in_review) and the
    // ci_remediations row is marked succeeded (CI went green momentarily,
    // then a new failure emerged on head-2).
    db.update_work_item(
        &rev_id,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();
    db.mark_ci_remediation_succeeded(&attempt_a.id, Some("head-2")).unwrap();

    // Chore returns to in_review.
    db.update_work_item(
        &chore,
        crate::work::WorkItemPatch {
            status: Some("in_review".into()),
            ..crate::work::WorkItemPatch::default()
        },
    )
    .unwrap();

    // Step 3: CI is failing on head-2 (R1's pushed commit). R1 is in_review
    // (not todo/active), so the secondary guard must NOT defer — a new attempt
    // for head-2 is the correct outcome.
    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "R1 in in_review must not count as in-flight",
    );

    let flipped = on_ci_failure_detected(
        &db,
        pub_.as_ref(),
        &fix_checker(),
        &candidate(&product, &chore, pr),
        &probe(pr, "head-2"),
        &one_failure(),
    )
    .await;
    assert!(
        flipped,
        "detection must proceed after R1 pushed — the prior worker completed its job",
    );

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tasks WHERE kind = 'revision' AND parent_task_id = ?1",
            rusqlite::params![&chore],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(revisions, 2, "second revision must be spawned for the new CI failure");
}
