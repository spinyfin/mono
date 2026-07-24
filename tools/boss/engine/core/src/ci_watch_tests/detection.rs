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

#[tokio::test]
async fn detection_defers_when_active_conflict_resolution_exists() {
    // §Q7 composed ordering: a conflict resolution attempt for
    // the same PR pre-empts the CI flow.
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/12";
    let (product, chore) = make_in_review(&db, "C-defer-cr", pr);
    db.mark_chore_blocked_merge_conflict(&chore, pr).unwrap();
    db.insert_conflict_resolution(crate::work::ConflictResolutionInsertInput {
        product_id: product.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 12,
        head_branch: "feature".into(),
        base_branch: "main".into(),
        base_sha_at_trigger: Some("base-1".into()),
        head_sha_before: Some("head-1".into()),
    })
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
    assert!(!flipped, "active conflict-resolution must pre-empt CI flow");
    let (status, _) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview, "row stays where it was");
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

    // 2. Retire — CI is back to clean.
    let resolved = on_ci_resolved(&db, pub_.as_ref(), &candidate(&product, &chore, pr), &[]).await;
    assert!(resolved);
    let (status, reason) = chore_state(&db, &chore);
    assert_eq!(status, TaskStatus::InReview);
    assert!(reason.is_none());

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
/// without incorrectly setting the `ci auto-fixed` badge. (T606 fix)
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

/// Regression for the operator-reported "stale badge from prior run" scenario:
/// a push to the PR changes the head SHA while the prior run's `ci_remediations`
/// row is still `pending`. The new CI run is all-in-flight (no failing leaf).
///
/// Before the fix, `on_ci_in_flight_supersedes_failure` bailed when it found the
/// pending row (even though it was for the old head SHA), leaving the macOS badge
/// stuck at "ci failing". After the fix the stale row is abandoned and
/// `CiFailureCleared` is emitted so the badge correctly reflects the new run.
///
/// This is the scenario the operator described: "they were all in progress, but it
/// was showing a stale badge. I don't think the original shake was actually based
/// on things that had one test failing."
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

#[test]
fn encode_failed_checks_round_trip() {
    let json = encode_failed_checks(&[RequiredCheckFailure {
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
