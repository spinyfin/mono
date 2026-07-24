//! CI-remediation lifecycle: listing/filtering, budget snapshot and
//! effective-budget resolution, attempts-used accounting, flaky-retrigger
//! signalling, and the ci_failure / merge_conflict / validated-green
//! block-and-signal state machines.

use super::*;

/// `list_ci_remediations` honours the `(product, status, work_item)`
/// filter triple AND-ed and orders rows freshest-first. The empty
/// filter set returns every row; `status = []` matches every
/// status.
#[test]
fn list_ci_remediations_filters_and_orders_freshest_first() {
    let (_dir, path) = disk_db_path("list-ci-remediations-filters");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:foo/bar.git"));
    let chore_a = create_test_chore_manual(&db, product.id.clone(), "chore-a");
    let chore_b = create_test_chore_manual(&db, product.id.clone(), "chore-b");
    // Two rows for chore_a with different attempt_kinds + statuses.
    let r1 = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore_a.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/100".into(),
            pr_number: 100,
            head_branch: "feature-a".into(),
            head_sha_at_trigger: "head-a-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");
    let r2 = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore_a.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/100".into(),
            pr_number: 100,
            head_branch: "feature-a".into(),
            head_sha_at_trigger: "head-a-2".into(),
            attempt_kind: "retrigger".into(),
            consumes_budget: 0,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");
    // One row for chore_b.
    let r3 = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product.id.clone(),
            work_item_id: chore_b.id.clone(),
            pr_url: "https://github.com/foo/bar/pull/101".into(),
            pr_number: 101,
            head_branch: "feature-b".into(),
            head_sha_at_trigger: "head-b-1".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");
    db.mark_ci_remediation_failed(&r1.id, "boom").unwrap();

    // No filters → every row, freshest first.
    let all = db.list_ci_remediations(None, &[], None, None).unwrap();
    assert_eq!(all.len(), 3);
    // Most-recently-inserted row should be first.
    assert_eq!(all[0].id, r3.id);

    // Filter by product.
    let by_product = db.list_ci_remediations(Some(&product.id), &[], None, None).unwrap();
    assert_eq!(by_product.len(), 3);

    // Filter by work item.
    let by_item = db.list_ci_remediations(None, &[], Some(&chore_a.id), None).unwrap();
    assert_eq!(by_item.len(), 2);
    for row in &by_item {
        assert_eq!(row.work_item_id, chore_a.id);
    }

    // Filter by status: `failed` matches only r1.
    let failed_rows = db.list_ci_remediations(None, &["failed".into()], None, None).unwrap();
    assert_eq!(failed_rows.len(), 1);
    assert_eq!(failed_rows[0].id, r1.id);

    // Limit caps the row set.
    let capped = db.list_ci_remediations(None, &[], None, Some(2)).unwrap();
    assert_eq!(capped.len(), 2);

    // Compound: product + work_item + status, intersected.
    let intersect = db
        .list_ci_remediations(Some(&product.id), &["pending".into()], Some(&chore_a.id), None)
        .unwrap();
    assert_eq!(intersect.len(), 1);
    assert_eq!(intersect[0].id, r2.id);

    let _ = std::fs::remove_file(path);
}

/// `ci_budget_snapshot` joins `tasks.ci_attempt_budget` with the
/// product's `ci_attempt_budget` to produce the effective budget,
/// reads `ci_attempts_used`, and clamps the effective value to
/// `0..=10`. `blocked_reason` is reported only when the task is
/// currently `status='blocked'`.
#[test]
fn ci_budget_snapshot_combines_override_and_product_default() {
    let (_dir, path) = disk_db_path("ci-budget-snapshot");
    let db = WorkDb::open(path.clone()).unwrap();
    let product = create_test_product_with_repo(&db, "P", Some("git@github.com:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), "chore-budget");

    // Defaults: no per-PR override, product default = 3, used = 0.
    let snap = db.ci_budget_snapshot(&chore.id).unwrap().unwrap();
    assert_eq!(snap.per_pr_override, None);
    assert_eq!(snap.product_default, 3);
    assert_eq!(snap.effective, 3);
    assert_eq!(snap.used, 0);
    assert_eq!(snap.blocked_reason, None);

    // Override path: `set_ci_attempt_budget` clamps to `0..=10`.
    let snap = db.set_ci_attempt_budget(&chore.id, Some(7)).unwrap().unwrap();
    assert_eq!(snap.per_pr_override, Some(7));
    assert_eq!(snap.effective, 7);
    // Out-of-range value clamps.
    let snap = db.set_ci_attempt_budget(&chore.id, Some(25)).unwrap().unwrap();
    assert_eq!(snap.per_pr_override, Some(10));
    assert_eq!(snap.effective, 10);
    // Clear path → product default applies.
    let snap = db.set_ci_attempt_budget(&chore.id, None).unwrap().unwrap();
    assert_eq!(snap.per_pr_override, None);
    assert_eq!(snap.effective, 3);

    // Unknown work item.
    assert!(db.ci_budget_snapshot("chr_does_not_exist").unwrap().is_none());

    let _ = std::fs::remove_file(path);
}

/// Attempts-used accounting: `get_ci_attempts_used` reports 0 for a
/// fresh row, `increment_ci_attempts_used` accumulates one bump per
/// call (and the running total surfaces in the budget snapshot's
/// `used` field), and `reset_ci_attempts_used` zeroes it back out.
/// Both writers guard on `deleted_at IS NULL`, so neither touches a
/// soft-deleted row — the read path (which has no such guard) proves
/// the last live value survives unchanged.
#[test]
fn ci_attempts_used_increment_and_reset_accounting() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "attempts-accounting");
    let pr = "https://github.com/spinyfin/mono/pull/903";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // Default: a fresh row reports zero attempts used.
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

    // Each increment bumps the running total by exactly one.
    db.increment_ci_attempts_used(&chore).unwrap();
    db.increment_ci_attempts_used(&chore).unwrap();
    db.increment_ci_attempts_used(&chore).unwrap();
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        3,
        "three increments accumulate to a running total of 3",
    );
    // The same counter surfaces through the budget snapshot.
    assert_eq!(
        db.ci_budget_snapshot(&chore).unwrap().unwrap().used,
        3,
        "the snapshot's `used` field reflects the accumulated counter",
    );

    // reset zeroes the counter back out...
    db.reset_ci_attempts_used(&chore).unwrap();
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        0,
        "reset returns the running total to zero",
    );
    // ...and is idempotent on an already-zero counter.
    db.reset_ci_attempts_used(&chore).unwrap();
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 0);

    // Both writers guard on `deleted_at IS NULL`. Bump once, soft-delete
    // the row, then confirm a further increment / reset are no-ops: the
    // read path (no deleted_at guard) still sees the last live value.
    db.increment_ci_attempts_used(&chore).unwrap();
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
            params![chore, now_string()],
        )
        .unwrap();
    }
    db.increment_ci_attempts_used(&chore).unwrap();
    db.reset_ci_attempts_used(&chore).unwrap();
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        1,
        "increment/reset are no-ops on a soft-deleted row",
    );
}

/// `mark_ci_remediation_retriggered` flips the attempt to the terminal
/// `retriggered` status, records the flaky verdict, and stamps the
/// `ci_flaky_retriggered` signal on the parent WITHOUT moving it to
/// `status='blocked'`. The signal is what the completion path consults to
/// park the worker instead of looping. Idempotent on a re-marker.
#[test]
fn mark_ci_remediation_retriggered_records_flaky_signal_without_blocking() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "flaky");
    let pr = "https://github.com/spinyfin/mono/pull/71";
    let chore = make_in_review_chore(&db, &product_id, pr);

    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 71,
            head_branch: "boss/exec".into(),
            head_sha_at_trigger: "head-1".into(),
            attempt_kind: "retrigger".into(),
            consumes_budget: 0,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");

    // No flaky signal before the marker.
    assert!(!db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());

    let updated = db
        .mark_ci_remediation_retriggered(&attempt.id)
        .unwrap()
        .expect("retrigger flip");
    assert_eq!(updated.status, "retriggered");
    assert_eq!(updated.triage_class.as_deref(), Some("flaky_or_infra"));
    assert!(updated.finished_at.is_some());

    // Signal is active and FK-linked to the attempt.
    assert!(db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());
    let signals = db.active_blocked_signals(&chore).unwrap();
    let flaky = signals
        .iter()
        .find(|s| s.reason == "ci_flaky_retriggered")
        .expect("flaky signal present");
    assert_eq!(flaky.attempt_id.as_deref(), Some(attempt.id.as_str()));

    // The parent is NOT moved to blocked — it stays in_review.
    let task = match db.get_work_item(&chore).unwrap() {
        WorkItem::Chore(t) => t,
        other => panic!("expected chore, got {other:?}"),
    };
    assert_eq!(task.status, TaskStatus::InReview);
    assert!(task.blocked_reason.is_none());

    // The attempt is now terminal, so `active_ci_remediation_for_work_item`
    // (pending/running only) no longer returns it — the on-Stop catch-all
    // finalizer becomes a no-op and cannot re-mark it failed.
    assert!(db.active_ci_remediation_for_work_item(&chore).unwrap().is_none());

    // Idempotent: a duplicate marker is a no-op (row already terminal).
    assert!(db.mark_ci_remediation_retriggered(&attempt.id).unwrap().is_none());
    assert_eq!(
        db.active_blocked_signals(&chore)
            .unwrap()
            .iter()
            .filter(|s| s.reason == "ci_flaky_retriggered")
            .count(),
        1,
        "duplicate marker must not double-arm the signal",
    );
}

/// The `ci_flaky_retriggered` signal is cleared both when CI resolves
/// (`clear_ci_failure_signal_only`) and when a fresh remediation attempt
/// supersedes the verdict (`insert_ci_remediation`).
#[test]
fn ci_flaky_retrigger_signal_clears_on_resolve_and_supersede() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "flaky-clear");
    let pr = "https://github.com/spinyfin/mono/pull/72";
    let chore = make_in_review_chore(&db, &product_id, pr);

    let arm = |head: &str| -> String {
        let a = db
            .insert_ci_remediation(CiRemediationInsertInput {
                product_id: product_id.clone(),
                work_item_id: chore.clone(),
                pr_url: pr.into(),
                pr_number: 72,
                head_branch: "boss/exec".into(),
                head_sha_at_trigger: head.into(),
                attempt_kind: "retrigger".into(),
                consumes_budget: 0,
                failed_checks: "[]".into(),
                failure_kind: "pr_branch_ci".into(),
                before_commit_sha: None,
            })
            .unwrap()
            .expect("insert");
        db.mark_ci_remediation_retriggered(&a.id).unwrap().expect("flip");
        a.id
    };

    // Path 1: CI resolves while the parent stayed in_review.
    arm("head-a");
    assert!(db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());
    assert!(db.clear_ci_failure_signal_only(&chore).unwrap());
    assert!(!db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());

    // Path 2: a fresh remediation attempt supersedes the stale verdict.
    arm("head-b");
    assert!(db.has_active_ci_flaky_retrigger_signal(&chore).unwrap());
    db.insert_ci_remediation(CiRemediationInsertInput {
        product_id: product_id.clone(),
        work_item_id: chore.clone(),
        pr_url: pr.into(),
        pr_number: 72,
        head_branch: "boss/exec".into(),
        head_sha_at_trigger: "head-c".into(),
        attempt_kind: "fix".into(),
        consumes_budget: 1,
        failed_checks: "[]".into(),
        failure_kind: "pr_branch_ci".into(),
        before_commit_sha: None,
    })
    .unwrap()
    .expect("fresh insert");
    assert!(
        !db.has_active_ci_flaky_retrigger_signal(&chore).unwrap(),
        "a new remediation attempt must supersede the stale flaky verdict",
    );
}

/// Helper: collect the `reason` of every active blocked signal for a
/// work item (cleared signals are excluded by `active_blocked_signals`).
#[cfg(test)]
fn active_signal_reasons(db: &WorkDb, work_item_id: &str) -> Vec<String> {
    db.active_blocked_signals(work_item_id)
        .unwrap()
        .into_iter()
        .map(|s| s.reason)
        .collect()
}

/// Helper: read a chore/task work item and unwrap to its `Task`.
#[cfg(test)]
fn task_of(db: &WorkDb, work_item_id: &str) -> Task {
    match db.get_work_item(work_item_id).unwrap() {
        WorkItem::Chore(t) | WorkItem::Task(t) => t,
        other => panic!("expected a chore/task work item, got {other:?}"),
    }
}

/// `effective_ci_budget` resolves the per-PR override first, falls back
/// to the parent product's default, returns the hard default `3` for an
/// unknown work item, and clamps the resolved value to `0..=10`.
///
/// The unknown-item branch deliberately diverges from
/// `ci_budget_snapshot` (which returns `None`); this test pins both
/// behaviours side by side so the divergence is intentional, not drift.
#[test]
fn effective_ci_budget_resolves_override_default_and_clamps() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "eff-budget");
    let pr = "https://github.com/spinyfin/mono/pull/900";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // (b) No per-PR override → the product default applies. Use a value
    // that is NOT the documented hard default of 3, so a regression that
    // ignores the product column and hard-codes 3 would be caught.
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE products SET ci_attempt_budget = 5 WHERE id = ?1", [&product_id])
            .unwrap();
    }
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        5,
        "falls back to the product default when no per-PR override is set",
    );

    // (a) A per-PR override wins over the product default.
    db.set_ci_attempt_budget(&chore, Some(7))
        .unwrap()
        .expect("override write");
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        7,
        "the per-PR override takes precedence over the product default",
    );

    // (d) The resolved value is clamped to `0..=10`. `set_ci_attempt_budget`
    // already clamps on write, so poke the raw column past the bounds to
    // exercise the read-side clamp inside `effective_ci_budget` itself.
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET ci_attempt_budget = 99 WHERE id = ?1", [&chore])
            .unwrap();
    }
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        10,
        "an over-cap override clamps up to the hard ceiling of 10",
    );
    {
        let conn = db.connect().unwrap();
        conn.execute("UPDATE tasks SET ci_attempt_budget = -4 WHERE id = ?1", [&chore])
            .unwrap();
    }
    assert_eq!(
        db.effective_ci_budget(&chore).unwrap(),
        0,
        "a negative override clamps up to the floor of 0",
    );

    // (c) Unknown work item → the hard default of 3, which diverges from
    // `ci_budget_snapshot` returning `None` for the same missing id.
    assert_eq!(
        db.effective_ci_budget("chr_does_not_exist").unwrap(),
        3,
        "an unknown work item returns the hard default budget of 3",
    );
    assert!(
        db.ci_budget_snapshot("chr_does_not_exist").unwrap().is_none(),
        "ci_budget_snapshot diverges: it returns None for the same unknown item",
    );
}

/// Full CI-failure block lifecycle through the public API:
/// `mark_chore_blocked_ci_failure` blocks the parent and arms the
/// `ci_failure` signal; `clear_chore_blocked_ci_failure` flips it back
/// to `in_review` and clears the signal; and
/// `rearm_blocked_ci_failure_signal` reactivates a signal that was
/// cleared out from under a still-blocked parent (while staying a no-op
/// when the parent is no longer blocked). Assertions observe the public
/// task status / active-signal projection, never internal columns.
#[test]
fn ci_failure_block_mark_clear_rearm_lifecycle() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "ci-block-lifecycle");
    let pr = "https://github.com/spinyfin/mono/pull/901";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // mark: in_review → blocked: ci_failure, with the signal armed.
    db.mark_chore_blocked_ci_failure(&chore, pr, None)
        .unwrap()
        .expect("flip to blocked: ci_failure");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::Blocked);
    assert_eq!(t.blocked_reason.as_deref(), Some("ci_failure"));
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);

    // clear: blocked → in_review, and the active signal is cleared too.
    db.clear_chore_blocked_ci_failure(&chore, pr)
        .unwrap()
        .expect("clear the ci_failure block");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::InReview);
    assert!(t.blocked_reason.is_none());
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "clearing the block also clears the active ci_failure signal",
    );

    // rearm is a no-op while the parent is not blocked.
    assert!(
        !db.rearm_blocked_ci_failure_signal(&chore).unwrap(),
        "rearm must not arm a signal on a parent that is no longer blocked",
    );
    assert!(active_signal_reasons(&db, &chore).is_empty());

    // Re-block, then simulate a premature polymorphic clear that drops the
    // signal row but leaves the parent blocked. rearm must reactivate it.
    db.mark_chore_blocked_ci_failure(&chore, pr, None)
        .unwrap()
        .expect("re-block: ci_failure");
    assert!(
        db.clear_ci_failure_signal_only(&chore).unwrap(),
        "signal-only clear deactivates the signal",
    );
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "the signal is inactive after the signal-only clear",
    );
    assert_eq!(
        task_of(&db, &chore).status,
        TaskStatus::Blocked,
        "the parent stays blocked through a signal-only clear",
    );
    assert!(
        db.rearm_blocked_ci_failure_signal(&chore).unwrap(),
        "rearm reactivates a cleared signal while the parent is still blocked",
    );
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);
}

/// The validated-green "no CI to fix" retire
/// (`mark_ci_remediation_validated_green`) is the DB half of the
/// `boss engine ci mark-noop` gate. It atomically flips the attempt to
/// `succeeded` (stamping the validated head SHA + a `validated_green`
/// audit discriminator), unblocks the parent out of the CI-failure
/// loop, clears the signal, and resets the per-PR attempt counter.
/// Idempotent on a terminal row.
#[test]
fn validated_green_noop_retires_attempt_and_unblocks_parent() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "ci-validated-green");
    let pr = "https://github.com/spinyfin/mono/pull/950";
    let chore = make_in_review_chore(&db, &product_id, pr);

    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 950,
            head_branch: "feature-x".into(),
            head_sha_at_trigger: "sha-old".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");

    // The badgering state: parent blocked: ci_failure, counter bumped,
    // attempt still pending.
    db.mark_chore_blocked_ci_failure(&chore, pr, None)
        .unwrap()
        .expect("block");
    db.increment_ci_attempts_used(&chore).unwrap();
    assert_eq!(task_of(&db, &chore).status, TaskStatus::Blocked);
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);
    assert_eq!(db.get_ci_attempts_used(&chore).unwrap(), 1);

    // Validated-green retire keyed to the CURRENT (advanced) head SHA.
    let retired = db
        .mark_ci_remediation_validated_green(&attempt.id, Some("sha-new-green"))
        .unwrap()
        .expect("validated-green flip");
    assert_eq!(retired.status, "succeeded");
    assert_eq!(retired.head_sha_after.as_deref(), Some("sha-new-green"));
    assert_eq!(
        retired.failure_reason.as_deref(),
        Some("validated_green"),
        "the audit discriminator marks this as a validated-green retire",
    );

    // Parent left the loop: in_review, no block, no active signal, fresh budget.
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::InReview);
    assert!(t.blocked_reason.is_none());
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "the validated-green retire clears the active ci_failure signal",
    );
    assert_eq!(
        db.get_ci_attempts_used(&chore).unwrap(),
        0,
        "a completed cycle resets the per-PR attempt counter",
    );

    // Idempotent: the row is terminal now, so a repeat is a no-op.
    assert!(
        db.mark_ci_remediation_validated_green(&attempt.id, Some("sha-new-green"))
            .unwrap()
            .is_none(),
        "a second validated-green call on a terminal row writes nothing",
    );
}

/// The validated-green retire also handles the in_review-with-revision
/// model, where the parent's status never moved to `blocked` — only the
/// in-flight `ci_failure` signal is armed. The retire must clear that
/// signal so the merge-poller's clear path does not re-fire.
#[test]
fn validated_green_noop_clears_in_flight_signal_without_blocked_status() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "ci-validated-green-inflight");
    let pr = "https://github.com/spinyfin/mono/pull/951";
    let chore = make_in_review_chore(&db, &product_id, pr);

    let attempt = db
        .insert_ci_remediation(CiRemediationInsertInput {
            product_id: product_id.clone(),
            work_item_id: chore.clone(),
            pr_url: pr.into(),
            pr_number: 951,
            head_branch: "feature-y".into(),
            head_sha_at_trigger: "sha-old".into(),
            attempt_kind: "fix".into(),
            consumes_budget: 1,
            failed_checks: "[]".into(),
            failure_kind: "pr_branch_ci".into(),
            before_commit_sha: None,
        })
        .unwrap()
        .expect("insert");

    // in_review-with-revision: the signal is armed but the status stays
    // in_review (the parent was never flipped to blocked).
    db.record_ci_failure_in_flight(&chore, &attempt.id).unwrap();
    assert_eq!(task_of(&db, &chore).status, TaskStatus::InReview);
    assert_eq!(active_signal_reasons(&db, &chore), vec!["ci_failure"]);

    db.mark_ci_remediation_validated_green(&attempt.id, Some("sha-green"))
        .unwrap()
        .expect("validated-green flip");

    assert_eq!(task_of(&db, &chore).status, TaskStatus::InReview);
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "the retire clears the in-flight signal even though status never blocked",
    );
}

/// Merge-conflict counterpart of the CI-failure lifecycle test, exercising
/// `clear_chore_blocked_merge_conflict` and
/// `rearm_blocked_merge_conflict_signal` (both live, previously untested):
/// mark → clear (block + signal) → no-op rearm when in_review →
/// re-block → signal-only clear → rearm reactivates.
#[test]
fn merge_conflict_block_mark_clear_rearm_lifecycle() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "mc-block-lifecycle");
    let pr = "https://github.com/spinyfin/mono/pull/902";
    let chore = make_in_review_chore(&db, &product_id, pr);

    // mark: in_review → blocked: merge_conflict, with the signal armed.
    db.mark_chore_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("flip to blocked: merge_conflict");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::Blocked);
    assert_eq!(t.blocked_reason.as_deref(), Some("merge_conflict"));
    assert_eq!(active_signal_reasons(&db, &chore), vec!["merge_conflict"]);

    // clear: blocked → in_review, and the active signal is cleared too.
    db.clear_chore_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("clear the merge_conflict block");
    let t = task_of(&db, &chore);
    assert_eq!(t.status, TaskStatus::InReview);
    assert!(t.blocked_reason.is_none());
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "clearing the block also clears the active merge_conflict signal",
    );

    // rearm is a no-op while the parent is not blocked.
    assert!(
        !db.rearm_blocked_merge_conflict_signal(&chore).unwrap(),
        "rearm must not arm a signal on a parent that is no longer blocked",
    );
    assert!(active_signal_reasons(&db, &chore).is_empty());

    // Re-block, drop only the signal (premature polymorphic clear), then
    // confirm rearm reactivates it while the parent stays blocked.
    db.mark_chore_blocked_merge_conflict(&chore, pr)
        .unwrap()
        .expect("re-block: merge_conflict");
    assert!(
        db.clear_merge_conflict_signal_only(&chore).unwrap(),
        "signal-only clear deactivates the signal",
    );
    assert!(
        active_signal_reasons(&db, &chore).is_empty(),
        "the signal is inactive after the signal-only clear",
    );
    assert_eq!(
        task_of(&db, &chore).status,
        TaskStatus::Blocked,
        "the parent stays blocked through a signal-only clear",
    );
    assert!(
        db.rearm_blocked_merge_conflict_signal(&chore).unwrap(),
        "rearm reactivates a cleared signal while the parent is still blocked",
    );
    assert_eq!(active_signal_reasons(&db, &chore), vec!["merge_conflict"]);
}
