use super::*;

// Behavioural coverage for the `work/blocking.rs` helpers that
// production drives but no test exercised directly:
//
// * `WorkDb::clear_chore_blocked_merge_conflict_for_attempt` — the
//   stricter auto-retire path whose WHERE clause additionally pins
//   `blocked_attempt_id = ?attempt_id` (design Q5), so the engine
//   only ever undoes *its own* blocked row.
// * `WorkDb::recanonicalize_blocked_merge_conflict` /
//   `recanonicalize_blocked_ci_failure` (shared
//   `recanonicalize_blocked_signal`) — re-claiming a stranded
//   NULL-reason `blocked` parent back into the standard
//   `blocked: merge_conflict` / `blocked: ci_failure` loop and
//   re-arming its `task_blocked_signals` row.
// * `WorkDb::task_blocked_reason` — the scalar-reason read used by the
//   merge-poller drift guard.
//
// These assert observable outcomes (returned `Task`, the `blocked_reason`
// scalar, and `task_blocked_signals` rows via the existing query helpers)
// rather than SQL shape, and are written so the guards are load-bearing:
// relax the attempt-id / reason / pr_url predicate and a test fails.

/// Stand up a product-with-repo + a chore flipped to
/// `blocked: merge_conflict` against `pr_url`, then insert a pending
/// conflict-resolution attempt for it — which stamps
/// `tasks.blocked_attempt_id` with the attempt id. Returns the chore id
/// and the freshly-inserted attempt. Mirrors the arrangement
/// `conflict_watch::on_conflict_detected` produces in production and the
/// t15 precedent.
fn seed_blocked_merge_conflict_with_attempt(
    label: &str,
    pr_url: &str,
    pr_number: i64,
    base_sha: &str,
) -> (WorkDb, String, ConflictResolution) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(format!("Chore {label}"))
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.update_work_item(
        &chore.id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    db.mark_chore_blocked_merge_conflict(&chore.id, pr_url).unwrap();

    let attempt = db
        .insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(product.id.clone())
                .work_item_id(chore.id.clone())
                .pr_url(pr_url)
                .pr_number(pr_number)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger(base_sha)
                .head_sha_before("head-before")
                .build(),
        )
        .unwrap()
        .expect("first insert must produce a pending attempt");
    (db, chore.id, attempt)
}

/// Create a product-with-repo + a chore, then force it directly into the
/// stranded `status='blocked'` / `blocked_reason IS NULL` state against
/// `pr_url` (the T795 strand `recanonicalize_blocked_signal` targets).
/// The raw UPDATE mirrors the t15 precedent of constructing an edge state
/// the public setters don't expose. Returns the chore id.
fn seed_null_reason_blocked_chore(label: &str, pr_url: &str) -> (WorkDb, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = db
        .create_chore(
            CreateChoreInput::builder()
                .product_id(product.id.clone())
                .name(format!("Chore {label}"))
                .autostart(false)
                .build(),
        )
        .unwrap();
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks
                SET status = 'blocked',
                    blocked_reason = NULL,
                    pr_url = ?2
              WHERE id = ?1",
            params![chore.id, pr_url],
        )
        .unwrap();
    (db, chore.id)
}

/// (status, blocked_reason, blocked_attempt_id) read straight from the
/// row so the assertions pin the observable parent state independent of
/// the projection layer.
fn parent_state(db: &WorkDb, task_id: &str) -> (String, Option<String>, Option<String>) {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT status, blocked_reason, blocked_attempt_id FROM tasks WHERE id = ?1",
            params![task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// The `cleared_at` value for the (single) `merge_conflict` signal row of
/// a work item. `None` means the row is still armed (active).
fn merge_conflict_signal_cleared_at(db: &WorkDb, work_item_id: &str) -> Option<String> {
    db.connect()
        .unwrap()
        .query_row(
            "SELECT cleared_at FROM task_blocked_signals
              WHERE work_item_id = ?1 AND reason = 'merge_conflict'",
            params![work_item_id],
            |r| r.get(0),
        )
        .unwrap()
}

// ── clear_chore_blocked_merge_conflict_for_attempt ──────────────────────

/// Happy path: when `work_item_id` + `pr_url` + `blocked_attempt_id` all
/// match, the stricter clear retires the parent to `in_review`, NULLs the
/// reason / attempt-id columns, and stamps the matching
/// `task_blocked_signals` row `cleared_at`.
#[test]
fn clear_for_attempt_retires_when_all_three_match() {
    let pr_url = "https://github.com/foo/bar/pull/1";
    let (db, chore_id, attempt) = seed_blocked_merge_conflict_with_attempt("clear-match", pr_url, 1, "sha-1");

    // Precondition: blocked on merge_conflict, attempt-id stamped, signal armed.
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(attempt_id.as_deref(), Some(attempt.id.as_str()));
    assert!(
        merge_conflict_signal_cleared_at(&db, &chore_id).is_none(),
        "signal must be armed before the clear"
    );

    let cleared = db
        .clear_chore_blocked_merge_conflict_for_attempt(&chore_id, pr_url, &attempt.id)
        .unwrap()
        .expect("all three keys match — the row must be cleared");
    assert_eq!(cleared.status, TaskStatus::InReview);
    assert_eq!(cleared.blocked_reason, None);

    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "in_review");
    assert_eq!(reason, None);
    assert_eq!(attempt_id, None, "blocked_attempt_id must be NULLed on retire");
    assert!(
        merge_conflict_signal_cleared_at(&db, &chore_id).is_some(),
        "the matching side-table signal row must be stamped cleared_at on success"
    );

    // Idempotent: a second call finds nothing to clear.
    assert!(
        db.clear_chore_blocked_merge_conflict_for_attempt(&chore_id, pr_url, &attempt.id)
            .unwrap()
            .is_none(),
        "repeat clear is a no-op once the row has been retired"
    );
}

/// The load-bearing guard: an `attempt_id` mismatch (the scenario the
/// stricter variant exists to protect against — a human re-flipped the
/// chore under a *different* attempt id) is a no-op even when
/// `work_item_id`, `pr_url`, and reason all match. `Ok(None)`, and every
/// column plus the armed signal is left untouched.
#[test]
fn clear_for_attempt_is_noop_on_attempt_mismatch() {
    let pr_url = "https://github.com/foo/bar/pull/2";
    let (db, chore_id, attempt) = seed_blocked_merge_conflict_with_attempt("clear-attempt-miss", pr_url, 2, "sha-2");

    let result = db
        .clear_chore_blocked_merge_conflict_for_attempt(&chore_id, pr_url, "crz_someone_elses")
        .unwrap();
    assert!(
        result.is_none(),
        "a mismatched attempt id must not clear the row (design Q5 guard)"
    );

    // Row and signal are entirely untouched.
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(
        attempt_id.as_deref(),
        Some(attempt.id.as_str()),
        "the original attempt id must survive a guard miss"
    );
    assert!(
        merge_conflict_signal_cleared_at(&db, &chore_id).is_none(),
        "the signal must stay armed when the attempt-id guard misses"
    );
}

/// A `pr_url` mismatch is likewise a no-op — the PR was re-pointed under
/// the engine, so the row is not ours to retire even with the right
/// attempt id.
#[test]
fn clear_for_attempt_is_noop_on_pr_url_mismatch() {
    let pr_url = "https://github.com/foo/bar/pull/3";
    let (db, chore_id, attempt) = seed_blocked_merge_conflict_with_attempt("clear-pr-miss", pr_url, 3, "sha-3");

    assert!(
        db.clear_chore_blocked_merge_conflict_for_attempt(
            &chore_id,
            "https://github.com/foo/bar/pull/999",
            &attempt.id,
        )
        .unwrap()
        .is_none(),
        "a mismatched pr_url must not clear the row"
    );
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert!(merge_conflict_signal_cleared_at(&db, &chore_id).is_none());
}

// ── recanonicalize_blocked_merge_conflict / _ci_failure ─────────────────

/// A stranded `blocked` parent with a NULL scalar reason and a matching
/// `pr_url` is re-claimed as `blocked: merge_conflict`: status stays
/// `blocked`, the scalar reason is filled in, and a fresh
/// `task_blocked_signals` row (attempt_id NULL) is armed. Idempotent — a
/// second call sees a non-NULL reason and is a no-op.
#[test]
fn recanonicalize_merge_conflict_reclaims_null_reason_blocked() {
    let pr_url = "https://github.com/foo/bar/pull/10";
    let (db, chore_id) = seed_null_reason_blocked_chore("recanon-mc", pr_url);

    // Precondition: no active signal for the stranded row.
    assert!(
        db.active_blocked_signals(&chore_id).unwrap().is_empty(),
        "the stranded row starts with no armed signal"
    );

    let updated = db
        .recanonicalize_blocked_merge_conflict(&chore_id, pr_url)
        .unwrap()
        .expect("NULL-reason blocked row with matching pr_url is re-claimable");
    assert_eq!(
        updated.status,
        TaskStatus::Blocked,
        "recanonicalise leaves status at blocked"
    );
    assert_eq!(updated.blocked_reason.as_deref(), Some("merge_conflict"));

    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));

    let signals = db.active_blocked_signals(&chore_id).unwrap();
    let mc = signals
        .iter()
        .find(|s| s.reason == "merge_conflict")
        .expect("a merge_conflict signal must be re-armed");
    assert_eq!(mc.attempt_id, None, "re-armed signal carries no attempt id");

    // Idempotent: reason is now non-NULL, so the WHERE guard misses.
    assert!(
        db.recanonicalize_blocked_merge_conflict(&chore_id, pr_url)
            .unwrap()
            .is_none(),
        "re-canonicalise must not re-fire once the scalar reason is set"
    );
}

/// CI analogue: the same stranded row is re-claimed as
/// `blocked: ci_failure` with a matching re-armed signal.
#[test]
fn recanonicalize_ci_failure_reclaims_null_reason_blocked() {
    let pr_url = "https://github.com/foo/bar/pull/11";
    let (db, chore_id) = seed_null_reason_blocked_chore("recanon-ci", pr_url);

    let updated = db
        .recanonicalize_blocked_ci_failure(&chore_id, pr_url)
        .unwrap()
        .expect("NULL-reason blocked row is re-claimable as ci_failure");
    assert_eq!(updated.status, TaskStatus::Blocked);
    assert_eq!(updated.blocked_reason.as_deref(), Some("ci_failure"));

    let signals = db.active_blocked_signals(&chore_id).unwrap();
    assert!(
        signals
            .iter()
            .any(|s| s.reason == "ci_failure" && s.attempt_id.is_none()),
        "a ci_failure signal must be re-armed"
    );

    // Idempotent.
    assert!(
        db.recanonicalize_blocked_ci_failure(&chore_id, pr_url)
            .unwrap()
            .is_none()
    );
}

/// No mutation when the scalar reason is already non-NULL: an owner
/// (`merge_conflict` here) set it, and re-canonicalise must never
/// overwrite a live owner's reason.
#[test]
fn recanonicalize_is_noop_when_reason_already_set() {
    let pr_url = "https://github.com/foo/bar/pull/12";
    let (db, chore_id, _attempt) = seed_blocked_merge_conflict_with_attempt("recanon-owned", pr_url, 12, "sha-12");

    // Try to re-canonicalise as ci_failure — the existing merge_conflict
    // reason must win.
    assert!(
        db.recanonicalize_blocked_ci_failure(&chore_id, pr_url)
            .unwrap()
            .is_none(),
        "a non-NULL reason blocks re-canonicalisation"
    );
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(
        reason.as_deref(),
        Some("merge_conflict"),
        "the live owner's reason must be preserved"
    );
}

/// No mutation when `pr_url` does not match: the PR was re-pointed, so
/// the stranded row is not the one this probe observed.
#[test]
fn recanonicalize_is_noop_when_pr_url_mismatch() {
    let (db, chore_id) = seed_null_reason_blocked_chore("recanon-prmiss", "https://github.com/foo/bar/pull/13");

    assert!(
        db.recanonicalize_blocked_merge_conflict(&chore_id, "https://github.com/foo/bar/pull/999")
            .unwrap()
            .is_none(),
        "a mismatched pr_url must not re-claim the row"
    );
    let (status, reason, _) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason, None, "the stranded row's NULL reason is left intact");
    assert!(
        db.active_blocked_signals(&chore_id).unwrap().is_empty(),
        "no signal is armed on a guard miss"
    );
}

// ── task_blocked_reason ─────────────────────────────────────────────────

/// `task_blocked_reason` returns the current scalar reason for a blocked
/// work item, `None` for a blocked row whose reason is NULL, and `None`
/// when the parent is not blocked at all.
#[test]
fn task_blocked_reason_reflects_current_scalar_reason() {
    // Blocked with a concrete reason → Some(reason).
    let pr_url = "https://github.com/foo/bar/pull/20";
    let (db, chore_id, _attempt) = seed_blocked_merge_conflict_with_attempt("reason-mc", pr_url, 20, "sha-20");
    assert_eq!(
        db.task_blocked_reason(&chore_id).unwrap().as_deref(),
        Some("merge_conflict")
    );

    // Blocked but NULL reason → None (the IS NOT NULL guard screens it out).
    let (db2, stranded) = seed_null_reason_blocked_chore("reason-null", "https://github.com/foo/bar/pull/21");
    assert_eq!(
        db2.task_blocked_reason(&stranded).unwrap(),
        None,
        "a NULL-reason blocked row reads as no reason"
    );

    // Not blocked → None even if a stale reason somehow lingered. Retire
    // the first chore back to in_review and confirm the read goes empty.
    db.clear_chore_blocked_merge_conflict(&chore_id, pr_url).unwrap();
    assert_eq!(parent_state(&db, &chore_id).0, "in_review");
    assert_eq!(
        db.task_blocked_reason(&chore_id).unwrap(),
        None,
        "an in_review task has no blocked reason"
    );
}
