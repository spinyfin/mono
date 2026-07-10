use super::*;

// Behaviour coverage for `WorkDb::has_in_flight_ci_fix_revision` in
// `work/blocking.rs` — the secondary pre-flight gate `ci_watch` uses to keep a
// new CI-fix revision from spawning while a prior revision's worker is still in
// flight. The predicate returns true when the chore has a live child that is a
// `kind='revision'`, `created_via LIKE 'ci-fix:%'`, `status IN
// (todo,active,blocked)`, `deleted_at IS NULL` row. Each test plants children
// in a specific shape (via the `insert_ci_fix_revision_row` /
// `insert_revision_row` helpers and raw SQL where a shape the helpers won't
// produce is needed) and asserts the observable boolean — never SQL internals.

/// Flip a task's `status` directly so the `status IN (todo,active,blocked)`
/// predicate can be exercised with any value, including terminal ones no
/// public flip produces on a bare revision row.
fn set_status(db: &WorkDb, task_id: &str, status: &str) {
    let conn = db.connect().unwrap();
    conn.execute("UPDATE tasks SET status = ?2 WHERE id = ?1", params![task_id, status])
        .unwrap();
}

/// Flip a task's `created_via` directly so the `LIKE 'ci-fix:%'` predicate can
/// be exercised with a non-CI-fix provenance (e.g. an `attention:`-created
/// revision).
fn set_created_via(db: &WorkDb, task_id: &str, created_via: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET created_via = ?2 WHERE id = ?1",
        params![task_id, created_via],
    )
    .unwrap();
}

/// Flip a task's `kind` directly so the `kind='revision'` predicate can be
/// exercised with a non-revision child that otherwise matches every filter.
fn set_kind(db: &WorkDb, task_id: &str, kind: &str) {
    let conn = db.connect().unwrap();
    conn.execute("UPDATE tasks SET kind = ?2 WHERE id = ?1", params![task_id, kind])
        .unwrap();
}

/// Soft-delete a task (stamp `deleted_at`) so the `deleted_at IS NULL`
/// predicate can be exercised.
fn soft_delete(db: &WorkDb, task_id: &str) {
    let conn = db.connect().unwrap();
    conn.execute(
        "UPDATE tasks SET deleted_at = ?2 WHERE id = ?1",
        params![task_id, now_string()],
    )
    .unwrap();
}

/// (1) A chore with no children at all → false. Nothing can be in flight.
#[test]
fn has_in_flight_ci_fix_revision_false_without_children() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "cifix-no-children");
    let chore = make_chore_root(&db, &product_id, "childless");

    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "a chore with no children has no in-flight CI-fix revision",
    );
}

/// (2) A live CI-fix revision child → true, for every one of the three live
/// statuses (`todo`, `active`, `blocked`) the predicate accepts.
#[test]
fn has_in_flight_ci_fix_revision_true_for_each_live_status() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "cifix-live");
    let chore = make_chore_root(&db, &product_id, "with-revision");
    // `insert_ci_fix_revision_row` plants a `kind='revision'`,
    // `created_via='ci-fix:<rem>'`, `deleted_at NULL` child (initially active).
    let revision = insert_ci_fix_revision_row(&db, &product_id, &chore, "rem_1");

    for status in ["todo", "active", "blocked"] {
        set_status(&db, &revision, status);
        assert!(
            db.has_in_flight_ci_fix_revision(&chore).unwrap(),
            "a CI-fix revision in status {status} counts as in flight",
        );
    }
}

/// (3) The CI-fix revision reaching a terminal status (`in_review`, `done`,
/// `cancelled`) drops it out of the live set → false. This is the gate's whole
/// point: once the worker has pushed and moved on, a fresh attempt may proceed.
#[test]
fn has_in_flight_ci_fix_revision_false_for_terminal_status() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "cifix-terminal");
    let chore = make_chore_root(&db, &product_id, "terminal-rev");
    let revision = insert_ci_fix_revision_row(&db, &product_id, &chore, "rem_2");

    for status in ["in_review", "done", "cancelled"] {
        set_status(&db, &revision, status);
        assert!(
            !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
            "a CI-fix revision in terminal status {status} is not in flight",
        );
    }
}

/// (4) A live revision whose `created_via` is not a `ci-fix:` provenance (e.g.
/// an `attention:`-created revision) → false: the predicate is scoped to
/// engine-triggered CI-fix revisions only.
#[test]
fn has_in_flight_ci_fix_revision_false_for_non_cifix_provenance() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "cifix-provenance");
    let chore = make_chore_root(&db, &product_id, "attention-rev");
    // A live `kind='revision'` child that matches every filter except
    // `created_via LIKE 'ci-fix:%'`.
    let revision = insert_ci_fix_revision_row(&db, &product_id, &chore, "rem_3");
    set_created_via(&db, &revision, "attention:att_123");

    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "a non-`ci-fix:` revision does not count, even while live",
    );

    // Sanity: restoring a `ci-fix:` provenance flips it back to true, proving
    // the created_via filter is what excluded it above.
    set_created_via(&db, &revision, "ci-fix:rem_3");
    assert!(db.has_in_flight_ci_fix_revision(&chore).unwrap());
}

/// (5) A matching CI-fix revision that is soft-deleted (`deleted_at` set) →
/// false: soft-deleted rows are not in flight.
#[test]
fn has_in_flight_ci_fix_revision_false_when_soft_deleted() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "cifix-deleted");
    let chore = make_chore_root(&db, &product_id, "deleted-rev");
    let revision = insert_ci_fix_revision_row(&db, &product_id, &chore, "rem_4");

    // Live and matching before the soft-delete.
    assert!(db.has_in_flight_ci_fix_revision(&chore).unwrap());

    soft_delete(&db, &revision);
    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "a soft-deleted CI-fix revision is not in flight",
    );
}

/// (6) A live child that matches status + provenance but is NOT
/// `kind='revision'` → false: the predicate is scoped to revision children.
#[test]
fn has_in_flight_ci_fix_revision_false_for_non_revision_kind() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product_id = make_revision_product(&db, "cifix-kind");
    let chore = make_chore_root(&db, &product_id, "non-revision-child");
    // Start from a matching CI-fix revision, then flip only its kind so the
    // sole failing predicate is `kind='revision'`.
    let child = insert_ci_fix_revision_row(&db, &product_id, &chore, "rem_5");
    assert!(
        db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "matches while a revision"
    );

    set_kind(&db, &child, "chore");
    assert!(
        !db.has_in_flight_ci_fix_revision(&chore).unwrap(),
        "a non-revision child does not count even with a live ci-fix provenance",
    );
}
