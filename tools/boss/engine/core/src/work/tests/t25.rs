use super::*;

// Behaviour coverage for the CI-remediation attempt-state mutators in
// `work/blocking.rs` that the coordinator spawn-flow and the on-Stop
// completion finalizer drive but that had no direct test. Each test plants a
// `ci_remediations` row via the public `insert_ci_remediation` seam, drives it
// through the public flip helpers, and asserts on the resulting row/signal
// state — the observable status flips, WHERE-guard misses (no-op on a terminal
// row / unknown id), and idempotency — never the SQL internals.

/// Insert a `pending` `ci_remediations` `fix` attempt for `work_item_id` and
/// return its id. The unique key is `(work_item_id, head_sha, attempt_kind)`,
/// so `head_sha` varies per call site to allow several attempts per work item.
fn seed_pending_remediation(db: &WorkDb, product_id: &str, work_item_id: &str, head_sha: &str) -> String {
    db.insert_ci_remediation(
        CiRemediationInsertInput::builder()
            .product_id(product_id)
            .work_item_id(work_item_id)
            .pr_url("https://github.com/spinyfin/mono/pull/1")
            .pr_number(1)
            .head_branch("feature")
            .head_sha_at_trigger(head_sha)
            .attempt_kind("fix")
            .consumes_budget(1)
            .failed_checks("[]")
            .failure_kind("pr_branch_ci")
            .build(),
    )
    .unwrap()
    .expect("insert must land a pending remediation")
    .id
}

/// Create a product and return its id. The crate has no public "get one
/// remediation" method, so each test observes state through the mutator's
/// return value — the freshly re-read `CiRemediation` row — rather than
/// re-querying.
fn make_product(db: &WorkDb, label: &str) -> String {
    create_test_product_named(db, &format!("Boss-{label}")).id
}

// ── mark_ci_remediation_running ────────────────────────────────────────────

/// Happy path: a `pending` attempt flips to `running`, stamps the
/// coordinator-owned spawn columns, and stamps `started_at`.
#[test]
fn mark_ci_remediation_running_flips_pending_and_stamps_spawn_columns() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-run-happy");
    let chore = create_test_chore(&db, product.clone(), "run-happy").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-run-1");

    let updated = db
        .mark_ci_remediation_running(&attempt, "lease-abc", "ws-42", "worker-7")
        .unwrap()
        .expect("pending attempt must flip to running");

    assert_eq!(updated.status, "running");
    assert_eq!(updated.cube_lease_id.as_deref(), Some("lease-abc"));
    assert_eq!(updated.cube_workspace_id.as_deref(), Some("ws-42"));
    assert_eq!(updated.worker_id.as_deref(), Some("worker-7"));
    assert!(
        updated.started_at.is_some(),
        "started_at should be stamped on the first run flip"
    );
}

/// Idempotent over a row already `running`: the guard accepts `running`, so a
/// re-spawn after engine restart re-stamps the spawn columns without rejecting.
#[test]
fn mark_ci_remediation_running_re_stamps_a_running_row() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-run-idem");
    let chore = create_test_chore(&db, product.clone(), "run-idem").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-run-2");

    let first = db
        .mark_ci_remediation_running(&attempt, "lease-1", "ws-1", "worker-1")
        .unwrap()
        .expect("first flip lands");
    let started = first.started_at.clone();

    // A second flip with fresh spawn metadata must succeed (guard accepts
    // `running`) and overwrite the coordinator columns.
    let second = db
        .mark_ci_remediation_running(&attempt, "lease-2", "ws-2", "worker-2")
        .unwrap()
        .expect("guard accepts an already-running row");

    assert_eq!(second.status, "running");
    assert_eq!(second.cube_lease_id.as_deref(), Some("lease-2"));
    assert_eq!(second.worker_id.as_deref(), Some("worker-2"));
    assert_eq!(
        second.started_at, started,
        "started_at is COALESCE-preserved across a re-stamp, not overwritten"
    );
}

/// Guard miss: a terminal (`abandoned`) row is not accepted by the
/// `status IN ('pending','running')` guard — the call is a no-op returning
/// `Ok(None)` and the row keeps its terminal state.
#[test]
fn mark_ci_remediation_running_no_ops_on_terminal_row() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-run-terminal");
    let chore = create_test_chore(&db, product.clone(), "run-terminal").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-run-3");

    db.mark_ci_remediation_abandoned(&attempt, "budget_exhausted")
        .unwrap()
        .expect("pending flips to abandoned");

    let result = db
        .mark_ci_remediation_running(&attempt, "lease-x", "ws-x", "worker-x")
        .unwrap();
    assert!(result.is_none(), "running flip must no-op on a terminal row");

    // The abandon stuck: re-reading via another guarded no-op confirms status.
    let reread = db.mark_ci_remediation_abandoned(&attempt, "again").unwrap();
    assert!(reread.is_none(), "row is already terminal");
}

// ── mark_ci_remediation_abandoned ──────────────────────────────────────────

/// Happy path: a `pending` attempt flips to `abandoned` with the reason and a
/// stamped `finished_at`.
#[test]
fn mark_ci_remediation_abandoned_flips_pending_and_stamps_reason() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-abandon-happy");
    let chore = create_test_chore(&db, product.clone(), "abandon-happy").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-abandon-1");

    let updated = db
        .mark_ci_remediation_abandoned(&attempt, "budget_exhausted")
        .unwrap()
        .expect("pending attempt must flip to abandoned");

    assert_eq!(updated.status, "abandoned");
    assert_eq!(updated.failure_reason.as_deref(), Some("budget_exhausted"));
    assert!(
        updated.finished_at.is_some(),
        "finished_at should be stamped on abandon"
    );
}

/// Idempotent / guard miss: a second abandon on an already-terminal row is a
/// no-op (`Ok(None)`) and does not overwrite the original reason. An unknown id
/// is likewise `Ok(None)`.
#[test]
fn mark_ci_remediation_abandoned_no_ops_on_terminal_and_unknown() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-abandon-idem");
    let chore = create_test_chore(&db, product.clone(), "abandon-idem").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-abandon-2");

    let first = db
        .mark_ci_remediation_abandoned(&attempt, "first_reason")
        .unwrap()
        .expect("first abandon lands");
    assert_eq!(first.failure_reason.as_deref(), Some("first_reason"));

    // Second abandon: guard misses on a terminal row, original reason preserved.
    let second = db.mark_ci_remediation_abandoned(&attempt, "second_reason").unwrap();
    assert!(second.is_none(), "abandon must no-op on an already-terminal row");

    // Unknown id: no row matches the guard.
    let missing = db.mark_ci_remediation_abandoned("cir-does-not-exist", "x").unwrap();
    assert!(missing.is_none(), "unknown id returns Ok(None)");
}

// ── abandon_active_ci_remediations_for_work_item ────────────────────────────

/// Happy path: every non-terminal (`pending`/`running`) attempt for the work
/// item flips to `abandoned` with `failure_reason='pr_merged'` in one shot; the
/// return count matches the number flipped.
#[test]
fn abandon_active_for_work_item_retires_all_non_terminal_attempts() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-abandon-all");
    let chore = create_test_chore(&db, product.clone(), "abandon-all").id;

    // One left pending, one moved to running: both are "active".
    let pending = seed_pending_remediation(&db, &product, &chore, "sha-all-1");
    let running = seed_pending_remediation(&db, &product, &chore, "sha-all-2");
    db.mark_ci_remediation_running(&running, "lease", "ws", "worker")
        .unwrap()
        .expect("second attempt is running");

    let count = db.abandon_active_ci_remediations_for_work_item(&chore).unwrap();
    assert_eq!(count, 2, "both the pending and running attempts are retired");

    // Both rows are now terminal with the merge reason — re-abandon no-ops.
    for id in [&pending, &running] {
        let reread = db.mark_ci_remediation_abandoned(id, "noop").unwrap();
        assert!(reread.is_none(), "attempt {id} should already be terminal");
    }
}

/// Guard miss / scoping: attempts already terminal are not touched, a work item
/// with no active attempts returns `0`, and a sibling work item's active
/// attempt is left untouched.
#[test]
fn abandon_active_for_work_item_skips_terminal_and_other_work_items() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-abandon-scope");
    let target = create_test_chore(&db, product.clone(), "abandon-target").id;
    let other = create_test_chore(&db, product.clone(), "abandon-other").id;

    // The target's only attempt is already terminal (failed) — not active.
    let terminal = seed_pending_remediation(&db, &product, &target, "sha-scope-1");
    db.mark_ci_remediation_failed(&terminal, "boom")
        .unwrap()
        .expect("pending flips to failed");

    // A sibling work item has a live pending attempt that must survive.
    let sibling = seed_pending_remediation(&db, &product, &other, "sha-scope-2");

    let count = db.abandon_active_ci_remediations_for_work_item(&target).unwrap();
    assert_eq!(count, 0, "no active attempts on the target → nothing retired");

    // The sibling's attempt is still active: it can be flipped to running.
    let still_active = db
        .mark_ci_remediation_running(&sibling, "lease", "ws", "worker")
        .unwrap();
    assert!(
        still_active.is_some(),
        "the other work item's attempt must be untouched and still non-terminal"
    );
}

// ── set_ci_remediation_log_excerpt ─────────────────────────────────────────

/// Happy path + overwrite: the log excerpt is stored on the attempt regardless
/// of status (the only guard is `id`), and a second call overwrites it.
#[test]
fn set_ci_remediation_log_excerpt_stores_and_overwrites() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-log");
    let chore = create_test_chore(&db, product.clone(), "log").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-log-1");

    let first = db
        .set_ci_remediation_log_excerpt(&attempt, "first tail")
        .unwrap()
        .expect("excerpt lands on a known attempt");
    assert_eq!(first.log_excerpt.as_deref(), Some("first tail"));

    let second = db
        .set_ci_remediation_log_excerpt(&attempt, "second tail")
        .unwrap()
        .expect("overwrite lands");
    assert_eq!(
        second.log_excerpt.as_deref(),
        Some("second tail"),
        "a second call overwrites the excerpt"
    );
}

/// Guard miss: an unknown attempt id matches no row and returns `Ok(None)`.
#[test]
fn set_ci_remediation_log_excerpt_no_ops_on_unknown_id() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let result = db.set_ci_remediation_log_excerpt("cir-nope", "tail").unwrap();
    assert!(result.is_none(), "unknown id returns Ok(None)");
}

// ── set_ci_remediation_triage_class ────────────────────────────────────────

/// Happy path + overwrite: the triage class is recorded and a later call
/// overwrites it (pure metadata, no state-machine effect).
#[test]
fn set_ci_remediation_triage_class_stores_and_overwrites() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-triage");
    let chore = create_test_chore(&db, product.clone(), "triage").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-triage-1");

    let first = db
        .set_ci_remediation_triage_class(&attempt, "tractable")
        .unwrap()
        .expect("triage class lands");
    assert_eq!(first.triage_class.as_deref(), Some("tractable"));
    // Setting triage class must not disturb the row's status.
    assert_eq!(first.status, "pending");

    let second = db
        .set_ci_remediation_triage_class(&attempt, "flaky_or_infra")
        .unwrap()
        .expect("overwrite lands");
    assert_eq!(second.triage_class.as_deref(), Some("flaky_or_infra"));
}

/// Guard miss: an unknown attempt id returns `Ok(None)`.
#[test]
fn set_ci_remediation_triage_class_no_ops_on_unknown_id() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let result = db.set_ci_remediation_triage_class("cir-nope", "tractable").unwrap();
    assert!(result.is_none(), "unknown id returns Ok(None)");
}

// ── clear_ci_flaky_retrigger_signal ────────────────────────────────────────

/// Happy path: an active `ci_flaky_retriggered` signal (armed by a retrigger)
/// is cleared, the call reports one row cleared, and the signal is no longer
/// active.
#[test]
fn clear_ci_flaky_retrigger_signal_clears_an_active_signal() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-clear-happy");
    let chore = create_test_chore(&db, product.clone(), "clear-happy").id;
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-clear-1");

    // A retrigger arms the flaky signal on the parent work item.
    db.mark_ci_remediation_retriggered(&attempt)
        .unwrap()
        .expect("pending attempt flips to retriggered");
    assert!(
        db.has_active_ci_flaky_retrigger_signal(&chore).unwrap(),
        "retrigger should arm the flaky signal"
    );

    let cleared = db.clear_ci_flaky_retrigger_signal(&chore).unwrap();
    assert_eq!(cleared, 1, "exactly the one active signal is cleared");
    assert!(
        !db.has_active_ci_flaky_retrigger_signal(&chore).unwrap(),
        "signal must be inactive after clear"
    );
}

/// No-op / idempotency: clearing when no signal is active returns `0`, and a
/// second clear after one already succeeded also returns `0`.
#[test]
fn clear_ci_flaky_retrigger_signal_no_ops_when_none_active() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = make_product(&db, "cir-clear-noop");
    let chore = create_test_chore(&db, product.clone(), "clear-noop").id;

    // No signal was ever armed for this work item.
    assert_eq!(
        db.clear_ci_flaky_retrigger_signal(&chore).unwrap(),
        0,
        "clearing with no active signal is a no-op"
    );

    // Arm and clear once, then confirm a repeat clear is idempotent.
    let attempt = seed_pending_remediation(&db, &product, &chore, "sha-clear-2");
    db.mark_ci_remediation_retriggered(&attempt).unwrap();
    assert_eq!(db.clear_ci_flaky_retrigger_signal(&chore).unwrap(), 1);
    assert_eq!(
        db.clear_ci_flaky_retrigger_signal(&chore).unwrap(),
        0,
        "a second clear finds nothing active"
    );
}
