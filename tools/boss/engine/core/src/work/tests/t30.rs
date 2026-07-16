use super::*;

// Behavioural coverage for the two `WorkDb` write methods in
// `work/conflict_res.rs` that production drives (from `conflict_ladder.rs`
// and `conflict_watch.rs`) but no test exercised directly:
//
// * `mark_conflict_resolution_succeeded_at_rung` — the pending/running →
//   `succeeded` transition, its `resolved_by_rung` COALESCE precedence
//   (a pre-stamp wins over a later mark, including the rung-3 wrapper
//   `mark_conflict_resolution_succeeded`), the `finished_at`/`head_sha_after`
//   COALESCE preservation, and the terminal-row no-op guard.
// * `invalidate_stale_succeeded_conflict_resolution` — the deliberately
//   `succeeded`-only transition to `failed` that frees the UNIQUE slot by
//   NULLing `base_sha_at_trigger`, clears `resolved_by_rung`, preserves an
//   existing `finished_at`, and no-ops on every non-`succeeded` status.
//
// The failed/abandoned paths clearing a premature `resolved_by_rung` stamp
// back to NULL (the other half of the rung-preservation contract) is
// asserted in `t04::premature_rung_stamp_is_cleared_on_failed_and_abandoned`;
// it is not duplicated here.
//
// All assertions pin the returned `ConflictResolution` and the
// `get_conflict_resolution` / `latest_conflict_resolution_for_work_item`
// reads rather than the SQL text.

/// Stand up a fresh in-memory `WorkDb` plus a product-with-repo and a
/// chore under it. Returns `(db, product_id, chore_id)`.
fn seed_product_and_chore(label: &str) -> (WorkDb, String, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), format!("Chore {label}"));
    (db, product.id, chore.id)
}

/// Insert a pending conflict-resolution attempt for `work_item_id` with a
/// distinct `base_sha` (part of the UNIQUE key, so each call lands its own
/// row). Returns the inserted attempt.
fn insert_attempt(db: &WorkDb, product_id: &str, work_item_id: &str, base_sha: &str) -> ConflictResolution {
    db.insert_conflict_resolution(
        ConflictResolutionInsertInput::builder()
            .product_id(product_id)
            .work_item_id(work_item_id)
            .pr_url(format!("https://github.com/foo/bar/pull/{base_sha}"))
            .pr_number(1)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger(base_sha)
            .head_sha_before("head-before")
            .build(),
    )
    .unwrap()
    .expect("insert must produce a pending attempt")
}

/// Directly plant `finished_at` / `head_sha_after` on a row so the
/// COALESCE-preservation assertions can exercise the "already set" branch
/// on a still-live (pending/running) attempt, which the public API never
/// populates before termination.
fn preset_row(db: &WorkDb, attempt_id: &str, finished_at: Option<&str>, head_sha_after: Option<&str>) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions
                SET finished_at = ?2, head_sha_after = ?3
              WHERE id = ?1",
            params![attempt_id, finished_at, head_sha_after],
        )
        .unwrap();
}

// ── mark_conflict_resolution_succeeded_at_rung ─────────────────────────

/// A pending attempt with no rung yet is flipped to `succeeded`, stamps
/// `resolved_by_rung = rung`, records `head_sha_after`, and gets a
/// `finished_at`. The same is observable through both read paths.
#[test]
fn mark_succeeded_at_rung_stamps_rung_on_unstamped_pending_row() {
    let (db, product, chore) = seed_product_and_chore("succeed-rung-pending");
    let attempt = insert_attempt(&db, &product, &chore, "sha-p");
    assert_eq!(attempt.status, "pending");
    assert_eq!(attempt.resolved_by_rung, None);
    assert!(attempt.finished_at.is_none());

    let succeeded = db
        .mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("head-after-1"), 1)
        .unwrap()
        .expect("succeeding a pending attempt returns the updated row");
    assert_eq!(succeeded.status, "succeeded");
    assert_eq!(succeeded.resolved_by_rung, Some(1));
    assert_eq!(succeeded.head_sha_after.as_deref(), Some("head-after-1"));
    assert!(succeeded.finished_at.is_some(), "a fresh finished_at is stamped");

    // Both reads see the same terminal, rung-1 row.
    let by_id = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(by_id.status, "succeeded");
    assert_eq!(by_id.resolved_by_rung, Some(1));
    let latest = db.latest_conflict_resolution_for_work_item(&chore).unwrap().unwrap();
    assert_eq!(latest.id, attempt.id);
    assert_eq!(latest.resolved_by_rung, Some(1));
    assert_eq!(latest.head_sha_after.as_deref(), Some("head-after-1"));
}

/// A `running` attempt is equally eligible (the guard is
/// `status IN ('pending','running')`), and rung 0 (a deterministic
/// resolver) is stamped verbatim.
#[test]
fn mark_succeeded_at_rung_succeeds_a_running_row_at_rung_zero() {
    let (db, product, chore) = seed_product_and_chore("succeed-rung-running");
    let attempt = insert_attempt(&db, &product, &chore, "sha-r");
    let running = db
        .mark_conflict_resolution_running(&attempt.id, "lease-1", "ws-1", "worker-1")
        .unwrap()
        .expect("pending → running returns the row");
    assert_eq!(running.status, "running");

    let succeeded = db
        .mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("head-after-r"), 0)
        .unwrap()
        .expect("succeeding a running attempt returns the updated row");
    assert_eq!(succeeded.status, "succeeded");
    assert_eq!(succeeded.resolved_by_rung, Some(0));
    assert_eq!(succeeded.head_sha_after.as_deref(), Some("head-after-r"));
}

/// `resolved_by_rung` COALESCE precedence: a row pre-stamped rung 2 (the
/// up-front rung-2 harness stamp) keeps `2` when a later mark specifies a
/// *different* rung — the earlier stamp wins, it is never overwritten.
#[test]
fn mark_succeeded_at_rung_preserves_prestamped_rung() {
    let (db, product, chore) = seed_product_and_chore("succeed-rung-prestamp");
    let attempt = insert_attempt(&db, &product, &chore, "sha-ps");
    let stamped = db
        .stamp_conflict_resolution_rung(&attempt.id, 2)
        .unwrap()
        .expect("stamp on a live attempt returns the updated row");
    assert_eq!(stamped.resolved_by_rung, Some(2));

    // A later mark at rung 1 must NOT clobber the earlier 2.
    let succeeded = db
        .mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("head-after"), 1)
        .unwrap()
        .expect("succeeding a pre-stamped attempt returns the row");
    assert_eq!(succeeded.status, "succeeded");
    assert_eq!(
        succeeded.resolved_by_rung,
        Some(2),
        "an earlier rung stamp wins over the rung the success mark specifies"
    );
}

/// The rung-3 wrapper `mark_conflict_resolution_succeeded` (which defaults
/// to `RUNG_FULL_WORKER`) also honours the COALESCE precedence: a row
/// pre-stamped rung 2 stays `2`, not `3`.
#[test]
fn mark_succeeded_wrapper_preserves_prestamped_rung_over_full_worker_default() {
    let (db, product, chore) = seed_product_and_chore("succeed-wrapper-prestamp");
    let attempt = insert_attempt(&db, &product, &chore, "sha-wrap");
    db.stamp_conflict_resolution_rung(&attempt.id, 2)
        .unwrap()
        .expect("stamp returns the row");

    let succeeded = db
        .mark_conflict_resolution_succeeded(&attempt.id, Some("head-after"))
        .unwrap()
        .expect("wrapper succeed returns the row");
    assert_eq!(succeeded.status, "succeeded");
    assert_eq!(
        succeeded.resolved_by_rung,
        Some(2),
        "the rung-3 wrapper must not overwrite an earlier rung-2 stamp"
    );
}

/// `finished_at` and `head_sha_after` are COALESCEd: an existing
/// `finished_at` is preserved (not bumped to the mark's `now`), and a
/// `None` `head_sha_after` does not erase a value already on the row.
#[test]
fn mark_succeeded_at_rung_coalesces_finished_at_and_head_sha() {
    let (db, product, chore) = seed_product_and_chore("succeed-coalesce");
    let attempt = insert_attempt(&db, &product, &chore, "sha-co");
    // Move to running (matches the live path) then plant a pre-existing
    // finished_at and head_sha_after directly — the public API never sets
    // these on a non-terminal row, but the COALESCE guards must honour them.
    db.mark_conflict_resolution_running(&attempt.id, "lease-1", "ws-1", "worker-1")
        .unwrap()
        .expect("pending → running");
    preset_row(&db, &attempt.id, Some("111"), Some("preset-head"));

    // Success mark passes head_sha_after = None: it must NOT overwrite the
    // planted value, and the planted finished_at must survive.
    let succeeded = db
        .mark_conflict_resolution_succeeded_at_rung(&attempt.id, None, 1)
        .unwrap()
        .expect("succeeding returns the row");
    assert_eq!(succeeded.status, "succeeded");
    assert_eq!(
        succeeded.finished_at.as_deref(),
        Some("111"),
        "an existing finished_at is preserved, not replaced with a fresh timestamp"
    );
    assert_eq!(
        succeeded.head_sha_after.as_deref(),
        Some("preset-head"),
        "a None head_sha_after must not erase an existing value"
    );
    assert_eq!(succeeded.resolved_by_rung, Some(1));
}

/// The transition is guarded `status IN ('pending','running')`: once the
/// row is terminal, a second mark (any rung) matches nothing and returns
/// `Ok(None)` without mutating the row. An unknown id is likewise `None`.
#[test]
fn mark_succeeded_at_rung_is_noop_on_terminal_rows() {
    let (db, product, chore) = seed_product_and_chore("succeed-terminal-noop");

    // Already succeeded: second mark is a no-op and does not change the rung.
    let a = insert_attempt(&db, &product, &chore, "sha-t1");
    db.mark_conflict_resolution_succeeded_at_rung(&a.id, Some("h"), 1)
        .unwrap()
        .expect("first succeed returns the row");
    let again = db
        .mark_conflict_resolution_succeeded_at_rung(&a.id, Some("h2"), 3)
        .unwrap();
    assert!(again.is_none(), "second succeed on a terminal row is a no-op");
    let reread = db.get_conflict_resolution(&a.id).unwrap().unwrap();
    assert_eq!(reread.resolved_by_rung, Some(1), "no-op must not restamp the rung");
    assert_eq!(
        reread.head_sha_after.as_deref(),
        Some("h"),
        "no-op must not change head_sha_after"
    );

    // Already failed: not eligible.
    let b = insert_attempt(&db, &product, &chore, "sha-t2");
    db.mark_conflict_resolution_failed(&b.id, "gave_up").unwrap().unwrap();
    assert!(
        db.mark_conflict_resolution_succeeded_at_rung(&b.id, None, 1)
            .unwrap()
            .is_none(),
        "a failed row cannot be succeeded"
    );
    assert_eq!(db.get_conflict_resolution(&b.id).unwrap().unwrap().status, "failed");

    // Already abandoned: not eligible.
    let c = insert_attempt(&db, &product, &chore, "sha-t3");
    db.mark_conflict_resolution_abandoned(&c.id, "parent_closed")
        .unwrap()
        .unwrap();
    assert!(
        db.mark_conflict_resolution_succeeded_at_rung(&c.id, None, 1)
            .unwrap()
            .is_none(),
        "an abandoned row cannot be succeeded"
    );
    assert_eq!(db.get_conflict_resolution(&c.id).unwrap().unwrap().status, "abandoned");

    // Unknown id.
    assert!(
        db.mark_conflict_resolution_succeeded_at_rung("crz_missing", None, 1)
            .unwrap()
            .is_none(),
        "unknown id yields Ok(None)"
    );
}

// ── invalidate_stale_succeeded_conflict_resolution ─────────────────────

/// The core stale-success invalidation: a `succeeded` row is flipped to
/// `failed` with the reason, its `base_sha_at_trigger` and
/// `resolved_by_rung` are NULLed (freeing the UNIQUE slot and dropping the
/// premature rung), and an existing `finished_at` is preserved.
#[test]
fn invalidate_stale_succeeded_flips_to_failed_and_clears_slot() {
    let (db, product, chore) = seed_product_and_chore("invalidate-basic");
    let attempt = insert_attempt(&db, &product, &chore, "base-sha-x");
    let succeeded = db
        .mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("head-after"), 1)
        .unwrap()
        .expect("succeed returns the row");
    assert_eq!(succeeded.status, "succeeded");
    assert_eq!(succeeded.resolved_by_rung, Some(1));
    assert_eq!(succeeded.base_sha_at_trigger.as_deref(), Some("base-sha-x"));
    let finished_before = succeeded.finished_at.clone();
    assert!(finished_before.is_some());

    let invalidated = db
        .invalidate_stale_succeeded_conflict_resolution(&attempt.id, "still_conflicting")
        .unwrap()
        .expect("invalidating a succeeded row returns the updated row");
    assert_eq!(invalidated.status, "failed");
    assert_eq!(invalidated.failure_reason.as_deref(), Some("still_conflicting"));
    assert_eq!(
        invalidated.base_sha_at_trigger, None,
        "base_sha_at_trigger is NULLed to free the UNIQUE slot"
    );
    assert_eq!(
        invalidated.resolved_by_rung, None,
        "the premature rung stamp is cleared on the failed transition"
    );
    assert_eq!(
        invalidated.finished_at, finished_before,
        "an existing finished_at is preserved (COALESCE)"
    );

    // The read paths agree.
    let by_id = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(by_id.status, "failed");
    assert_eq!(by_id.base_sha_at_trigger, None);
    let latest = db.latest_conflict_resolution_for_work_item(&chore).unwrap().unwrap();
    assert_eq!(latest.id, attempt.id);
    assert_eq!(latest.status, "failed");
}

/// After invalidation NULLs `base_sha_at_trigger`, `insert_conflict_resolution`
/// can land a fresh attempt at the very same `(base_sha, head_sha_before)`
/// key that the stale success was wedging — the whole point of the
/// primitive. Before invalidation the same insert idempotency-collides
/// (`Ok(None)`).
#[test]
fn invalidate_stale_succeeded_frees_the_reinsert_slot() {
    let (db, product, chore) = seed_product_and_chore("invalidate-reinsert");
    let attempt = insert_attempt(&db, &product, &chore, "wedge-sha");
    db.mark_conflict_resolution_succeeded_at_rung(&attempt.id, Some("h"), 1)
        .unwrap()
        .unwrap();

    // Same UNIQUE key while the succeeded row still holds it → collides.
    let blocked = db
        .insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(product.clone())
                .work_item_id(chore.clone())
                .pr_url("https://github.com/foo/bar/pull/wedge-sha")
                .pr_number(1)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger("wedge-sha")
                .head_sha_before("head-before")
                .build(),
        )
        .unwrap();
    assert!(blocked.is_none(), "a stale succeeded row wedges the re-insert slot");

    db.invalidate_stale_succeeded_conflict_resolution(&attempt.id, "still_conflicting")
        .unwrap()
        .unwrap();

    // Slot freed → a fresh pending attempt lands at the same key.
    let fresh = db
        .insert_conflict_resolution(
            ConflictResolutionInsertInput::builder()
                .product_id(product.clone())
                .work_item_id(chore.clone())
                .pr_url("https://github.com/foo/bar/pull/wedge-sha")
                .pr_number(1)
                .head_branch("feature")
                .base_branch("main")
                .base_sha_at_trigger("wedge-sha")
                .head_sha_before("head-before")
                .build(),
        )
        .unwrap()
        .expect("re-insert must produce a fresh pending attempt once the slot is freed");
    assert_eq!(fresh.status, "pending");
    assert_ne!(fresh.id, attempt.id, "the re-insert is a distinct row");
}

/// The transition is deliberately `status = 'succeeded'`-only. Every
/// non-succeeded status — pending, running, failed, abandoned — is a
/// no-op (`Ok(None)`) that leaves the row untouched, and an unknown id is
/// likewise `None`.
#[test]
fn invalidate_stale_succeeded_is_noop_on_non_succeeded_rows() {
    let (db, product, _) = seed_product_and_chore("invalidate-noop");
    // Each status gets its own chore so the churn guard (which pre-abandons
    // the 4th attempt under one work item within the window) never fires.
    let fresh_chore = |label: &str| create_test_chore_manual(&db, product.clone(), format!("Chore {label}")).id;

    // pending
    let pc = fresh_chore("pending");
    let pending = insert_attempt(&db, &product, &pc, "sha-pending");
    assert!(
        db.invalidate_stale_succeeded_conflict_resolution(&pending.id, "x")
            .unwrap()
            .is_none(),
        "a pending row is not invalidated"
    );
    let reread = db.get_conflict_resolution(&pending.id).unwrap().unwrap();
    assert_eq!(reread.status, "pending");
    assert_eq!(
        reread.base_sha_at_trigger.as_deref(),
        Some("sha-pending"),
        "no-op leaves the slot key intact"
    );

    // running
    let rc = fresh_chore("running");
    let running = insert_attempt(&db, &product, &rc, "sha-running");
    db.mark_conflict_resolution_running(&running.id, "lease", "ws", "worker")
        .unwrap()
        .unwrap();
    assert!(
        db.invalidate_stale_succeeded_conflict_resolution(&running.id, "x")
            .unwrap()
            .is_none(),
        "a running row is not invalidated"
    );
    assert_eq!(
        db.get_conflict_resolution(&running.id).unwrap().unwrap().status,
        "running"
    );

    // failed
    let fc = fresh_chore("failed");
    let failed = insert_attempt(&db, &product, &fc, "sha-failed");
    db.mark_conflict_resolution_failed(&failed.id, "gave_up")
        .unwrap()
        .unwrap();
    assert!(
        db.invalidate_stale_succeeded_conflict_resolution(&failed.id, "x")
            .unwrap()
            .is_none(),
        "a failed row is not re-invalidated"
    );
    assert_eq!(
        db.get_conflict_resolution(&failed.id)
            .unwrap()
            .unwrap()
            .failure_reason
            .as_deref(),
        Some("gave_up")
    );

    // abandoned
    let ac = fresh_chore("abandoned");
    let abandoned = insert_attempt(&db, &product, &ac, "sha-abandoned");
    db.mark_conflict_resolution_abandoned(&abandoned.id, "parent_closed")
        .unwrap()
        .unwrap();
    assert!(
        db.invalidate_stale_succeeded_conflict_resolution(&abandoned.id, "x")
            .unwrap()
            .is_none(),
        "an abandoned row is not invalidated"
    );
    assert_eq!(
        db.get_conflict_resolution(&abandoned.id).unwrap().unwrap().status,
        "abandoned"
    );

    // unknown id
    assert!(
        db.invalidate_stale_succeeded_conflict_resolution("crz_missing", "x")
            .unwrap()
            .is_none(),
        "unknown id yields Ok(None)"
    );
}
