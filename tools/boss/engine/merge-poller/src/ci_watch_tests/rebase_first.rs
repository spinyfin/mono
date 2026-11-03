use super::helpers::*;

// ----- Reconciled 2026-05-17 layered design call: rebase-first success ----

#[tokio::test]
async fn rebase_only_success_refunds_budget_slot() {
    let dir = tempdir().unwrap();
    let db = WorkDb::open(dir.path().join("boss.db")).unwrap();
    let pr = "https://github.com/foo/bar/pull/200";
    let (product, chore) = make_in_review(&db, "C-rebase", pr);
    let pub_ = Arc::new(RecordingPublisher::default());

    // 1. Detect a fix-kind failure — counter bumps to 1.
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
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // 2. Worker rebases onto base HEAD and reports green CI without
    //    a code change: rebase-only success path.
    let attempt = db
        .active_ci_remediation_for_work_item(&chore)
        .unwrap()
        .expect("attempt row");
    let updated = db
        .mark_ci_remediation_succeeded_via_rebase(&attempt.id, None)
        .unwrap()
        .expect("WHERE guard hit");

    assert_eq!(updated.status, "succeeded");
    assert_eq!(updated.consumes_budget, 0);
    assert_eq!(updated.failure_reason.as_deref(), Some("rebase_only"));

    // 3. Counter refunded: budget slot is NOT consumed.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

    // 4. Idempotent — repeat is a no-op.
    let again = db.mark_ci_remediation_succeeded_via_rebase(&attempt.id, None).unwrap();
    assert!(again.is_none(), "second call must be a no-op");
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);
}
