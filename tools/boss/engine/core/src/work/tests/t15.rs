use super::*;

// Behavioural coverage for the two conflict-resolution state-machine
// transitions in `work/conflict_res.rs` that production drives but no
// test exercised directly:
//
// * `WorkDb::retry_conflict_resolution` — the `failed`/`abandoned`
//   → `pending` re-arm plus the parent-task re-block.
// * `WorkDb::abandon_conflict_resolution_for_supersede` — the
//   same-base supersede abandon that NULLs `base_sha_at_trigger` to
//   free the `UNIQUE (work_item_id, base_sha_at_trigger)` slot.
//
// These assert observable outcomes (row status, cleared columns,
// parent kanban state, and whether a subsequent insert lands a fresh
// row) rather than SQL shape, and are written so the status guards and
// the slot-freeing NULL are load-bearing: drop either and a test fails.

/// Stand up a product-with-repo + a chore parked in
/// `blocked: merge_conflict` against `pr_url`, then insert a pending
/// conflict-resolution attempt for it. Returns the chore id and the
/// freshly-inserted attempt. Mirrors the arrangement
/// `conflict_watch::on_conflict_detected` produces in production.
fn seed_blocked_chore_with_attempt(
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

/// (status, blocked_reason, blocked_attempt_id) for a task, read
/// straight from the row so the assertions pin the observable parent
/// state independent of the projection layer.
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

// ── retry_conflict_resolution ───────────────────────────────────────────

/// The status guard is load-bearing: retry is a no-op returning
/// `Ok(None)` on every non-terminal status (`pending`, `running`,
/// `succeeded`), leaving the row untouched. Only `failed`/`abandoned`
/// are eligible.
#[test]
fn retry_is_noop_unless_failed_or_abandoned() {
    let (db, _chore, attempt) =
        seed_blocked_chore_with_attempt("retry-noop", "https://github.com/foo/bar/pull/1", 1, "sha-1");

    // pending → no-op.
    assert!(
        db.retry_conflict_resolution(&attempt.id).unwrap().is_none(),
        "retry on a pending attempt must be a no-op"
    );
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "pending"
    );

    // running → no-op.
    db.mark_conflict_resolution_running(&attempt.id, "lease-1", "ws-1", "worker-1")
        .unwrap()
        .expect("pending → running");
    assert!(
        db.retry_conflict_resolution(&attempt.id).unwrap().is_none(),
        "retry on a running attempt must be a no-op"
    );
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "running"
    );

    // succeeded → no-op.
    db.mark_conflict_resolution_succeeded(&attempt.id, Some("head-after"))
        .unwrap()
        .expect("running → succeeded");
    assert!(
        db.retry_conflict_resolution(&attempt.id).unwrap().is_none(),
        "retry on a succeeded attempt must be a no-op"
    );
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "succeeded"
    );

    // Unknown id → Ok(None).
    assert!(db.retry_conflict_resolution("crz_missing").unwrap().is_none());
}

/// A `failed` attempt carrying a full worker footprint (lease triple,
/// timestamps, failure reason, recorded head sha) is reset back to
/// `pending` with every one of those columns cleared.
#[test]
fn retry_resets_failed_attempt_and_clears_worker_footprint() {
    let (db, _chore, attempt) =
        seed_blocked_chore_with_attempt("retry-failed", "https://github.com/foo/bar/pull/2", 2, "sha-2");

    // Drive it through a real worker lifecycle so the lease triple and
    // started_at are stamped by production code, then fail it.
    db.mark_conflict_resolution_running(&attempt.id, "lease-x", "ws-x", "worker-x")
        .unwrap()
        .expect("pending → running");
    db.mark_conflict_resolution_failed(&attempt.id, "obsolescence_suspected")
        .unwrap()
        .expect("running → failed");
    // The worker can record a head sha before failing; stamp one so the
    // reset has something to clear (no production transition sets
    // head_sha_after on a failed row).
    db.connect()
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions SET head_sha_after = 'head-after' WHERE id = ?1",
            params![attempt.id],
        )
        .unwrap();

    // Precondition: everything is populated.
    let before = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(before.status, "failed");
    assert!(before.failure_reason.is_some());
    assert!(before.head_sha_after.is_some());
    assert!(before.cube_lease_id.is_some());
    assert!(before.cube_workspace_id.is_some());
    assert!(before.worker_id.is_some());
    assert!(before.started_at.is_some());
    assert!(before.finished_at.is_some());

    let reset = db
        .retry_conflict_resolution(&attempt.id)
        .unwrap()
        .expect("failed attempt is eligible for retry");
    assert_eq!(reset.status, "pending");
    assert_eq!(reset.failure_reason, None);
    assert_eq!(reset.head_sha_after, None);
    assert_eq!(reset.cube_lease_id, None);
    assert_eq!(reset.cube_workspace_id, None);
    assert_eq!(reset.worker_id, None);
    assert_eq!(reset.started_at, None);
    assert_eq!(reset.finished_at, None);
}

/// An `abandoned` attempt is equally eligible for retry: it too resets
/// to `pending`.
#[test]
fn retry_resets_abandoned_attempt() {
    let (db, _chore, attempt) =
        seed_blocked_chore_with_attempt("retry-abandoned", "https://github.com/foo/bar/pull/3", 3, "sha-3");
    db.mark_conflict_resolution_abandoned(&attempt.id, "parent_pr_closed")
        .unwrap()
        .expect("pending → abandoned");
    assert_eq!(
        db.get_conflict_resolution(&attempt.id).unwrap().unwrap().status,
        "abandoned"
    );

    let reset = db
        .retry_conflict_resolution(&attempt.id)
        .unwrap()
        .expect("abandoned attempt is eligible for retry");
    assert_eq!(reset.status, "pending");
    assert_eq!(reset.failure_reason, None);
    assert_eq!(reset.finished_at, None);
}

/// When the parent has already been retired to `in_review` (the
/// auto-retire path ran), retry flips it back to
/// `blocked: merge_conflict` and points `blocked_attempt_id` at the
/// reset row.
#[test]
fn retry_reblocks_parent_from_in_review() {
    let pr_url = "https://github.com/foo/bar/pull/4";
    let (db, chore_id, attempt) = seed_blocked_chore_with_attempt("retry-reblock", pr_url, 4, "sha-4");
    db.mark_conflict_resolution_running(&attempt.id, "lease-4", "ws-4", "worker-4")
        .unwrap();
    db.mark_conflict_resolution_failed(&attempt.id, "gave_up").unwrap();

    // Simulate the retire path putting the card back into review.
    db.update_work_item(
        &chore_id,
        WorkItemPatch {
            status: Some("in_review".into()),
            pr_url: Some(pr_url.into()),
            ..WorkItemPatch::default()
        },
    )
    .unwrap();
    assert_eq!(parent_state(&db, &chore_id).0, "in_review");

    let reset = db.retry_conflict_resolution(&attempt.id).unwrap().unwrap();
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(attempt_id.as_deref(), Some(reset.id.as_str()));
}

/// When the parent is still `blocked: merge_conflict` (retire never
/// ran because the conflict is still live), retry leaves the status
/// alone but re-points `blocked_attempt_id` at the reset row.
#[test]
fn retry_repoints_attempt_id_when_parent_already_blocked() {
    let (db, chore_id, attempt) =
        seed_blocked_chore_with_attempt("retry-repoint", "https://github.com/foo/bar/pull/5", 5, "sha-5");
    db.mark_conflict_resolution_failed(&attempt.id, "gave_up").unwrap();

    // Point the parent at a stale attempt id while it stays blocked, so
    // a successful re-point is observable as a change.
    db.connect()
        .unwrap()
        .execute(
            "UPDATE tasks SET blocked_attempt_id = 'crz_stale' WHERE id = ?1",
            params![chore_id],
        )
        .unwrap();
    assert_eq!(parent_state(&db, &chore_id).0, "blocked");

    let reset = db.retry_conflict_resolution(&attempt.id).unwrap().unwrap();
    let (status, reason, attempt_id) = parent_state(&db, &chore_id);
    assert_eq!(status, "blocked");
    assert_eq!(reason.as_deref(), Some("merge_conflict"));
    assert_eq!(
        attempt_id.as_deref(),
        Some(reset.id.as_str()),
        "blocked_attempt_id must be re-pointed off the stale id"
    );
}

// ── abandon_conflict_resolution_for_supersede ───────────────────────────

/// The status guard holds: supersede-abandon only touches
/// `pending`/`running` rows. On a terminal (`failed`/`succeeded`) row
/// it is a no-op returning `Ok(None)` and leaves `base_sha_at_trigger`
/// intact.
#[test]
fn abandon_for_supersede_is_noop_on_terminal_rows() {
    let (db, _chore, attempt) =
        seed_blocked_chore_with_attempt("supersede-noop", "https://github.com/foo/bar/pull/6", 6, "sha-6");
    db.mark_conflict_resolution_running(&attempt.id, "lease-6", "ws-6", "worker-6")
        .unwrap();
    db.mark_conflict_resolution_failed(&attempt.id, "gave_up").unwrap();

    assert!(
        db.abandon_conflict_resolution_for_supersede(&attempt.id, "superseded")
            .unwrap()
            .is_none(),
        "supersede-abandon must not touch a terminal row"
    );
    let row = db.get_conflict_resolution(&attempt.id).unwrap().unwrap();
    assert_eq!(row.status, "failed");
    assert_eq!(
        row.base_sha_at_trigger.as_deref(),
        Some("sha-6"),
        "the UNIQUE-slot key must survive an ineffective supersede-abandon"
    );

    // Unknown id → Ok(None).
    assert!(
        db.abandon_conflict_resolution_for_supersede("crz_missing", "x")
            .unwrap()
            .is_none()
    );
}

/// The differentiating behaviour: supersede-abandon NULLs
/// `base_sha_at_trigger`, freeing the `UNIQUE (work_item_id,
/// base_sha_at_trigger)` slot so a fresh attempt at the SAME base SHA
/// can be inserted — whereas a plain `mark_conflict_resolution_abandoned`
/// leaves the slot occupied and the same insert is idempotently
/// swallowed. The two branches share a setup and differ only in which
/// abandon they call, so this pins the slot-freeing NULL specifically.
#[test]
fn abandon_for_supersede_frees_unique_slot_unlike_plain_abandon() {
    const BASE: &str = "shared-base-sha";
    const PR: &str = "https://github.com/foo/bar/pull/7";

    // Control arm: plain abandon leaves base_sha_at_trigger in place,
    // so re-inserting at the same base SHA is a no-op (slot occupied).
    let (plain_db, plain_chore, plain_attempt) = seed_blocked_chore_with_attempt("supersede-plain", PR, 7, BASE);
    db_abandon_plain(&plain_db, &plain_attempt.id);
    assert_eq!(
        plain_db
            .get_conflict_resolution(&plain_attempt.id)
            .unwrap()
            .unwrap()
            .base_sha_at_trigger
            .as_deref(),
        Some(BASE),
        "plain abandon must NOT clear the UNIQUE-slot key"
    );
    let plain_reinsert = insert_same_base(&plain_db, &plain_chore, PR, 7, BASE);
    assert!(
        plain_reinsert.is_none(),
        "after a plain abandon the UNIQUE slot is still occupied — re-insert is swallowed"
    );

    // Supersede arm: same setup, but supersede-abandon NULLs the key,
    // so the identical re-insert lands a brand-new row.
    let (super_db, super_chore, super_attempt) = seed_blocked_chore_with_attempt("supersede-free", PR, 7, BASE);
    let abandoned = super_db
        .abandon_conflict_resolution_for_supersede(&super_attempt.id, "superseded_same_base")
        .unwrap()
        .expect("pending row is eligible for supersede-abandon");
    assert_eq!(abandoned.status, "abandoned");
    assert_eq!(
        abandoned.base_sha_at_trigger, None,
        "supersede-abandon must NULL the UNIQUE-slot key"
    );
    let fresh = insert_same_base(&super_db, &super_chore, PR, 7, BASE)
        .expect("freed slot must admit a fresh attempt at the same base SHA");
    assert_ne!(
        fresh.id, super_attempt.id,
        "the re-insert must be a distinct, fresh row"
    );
    assert_eq!(fresh.status, "pending");
    assert_eq!(fresh.base_sha_at_trigger.as_deref(), Some(BASE));
}

/// Plain engine-side abandon (base SHA unchanged path's counterpart).
fn db_abandon_plain(db: &WorkDb, attempt_id: &str) {
    db.mark_conflict_resolution_abandoned(attempt_id, "superseded_plain")
        .unwrap()
        .expect("pending → abandoned");
}

/// Re-run the detection-time insert for the same `(work_item, base_sha)`.
fn insert_same_base(
    db: &WorkDb,
    work_item_id: &str,
    pr_url: &str,
    pr_number: i64,
    base_sha: &str,
) -> Option<ConflictResolution> {
    let product_id: String = db
        .connect()
        .unwrap()
        .query_row(
            "SELECT product_id FROM tasks WHERE id = ?1",
            params![work_item_id],
            |r| r.get(0),
        )
        .unwrap();
    db.insert_conflict_resolution(
        ConflictResolutionInsertInput::builder()
            .product_id(product_id)
            .work_item_id(work_item_id)
            .pr_url(pr_url)
            .pr_number(pr_number)
            .head_branch("feature")
            .base_branch("main")
            .base_sha_at_trigger(base_sha)
            .head_sha_before("head-before")
            .build(),
    )
    .unwrap()
}
