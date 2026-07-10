use super::*;

// Behavioural coverage for the four read-side `WorkDb` query methods in
// `work/conflict_res.rs` that production drives but no test exercised:
//
// * `list_conflict_resolutions` — freshest-first ordering, the AND-ed
//   product/work-item/status filters, and the `limit` cap.
// * `latest_conflict_resolution_for_work_item` — returns the single most
//   recent attempt regardless of status, `None` when none exist.
// * `has_active_rebase_attempt_for_pr` — true only while a `rebase_attempts`
//   row is non-terminal (`pending`/`running`/`escalated`), false once it
//   reaches a terminal status, per-PR isolated, and false when the side
//   table has not been created at all.
// * `product_auto_pr_maintenance_enabled` — the column's boolean, defaulting
//   to enabled and reflecting an explicit toggle (and a missing product).
//
// All assertions pin observable return values and state transitions rather
// than the SQL text of the queries.

/// Stand up a fresh in-memory `WorkDb` plus a product-with-repo and a
/// chore under it. Returns `(db, product_id, chore_id)`.
fn seed_product_and_chore(label: &str) -> (WorkDb, String, String) {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let product = create_test_product_with_repo(&db, label, Some("git@example.invalid:foo/bar.git"));
    let chore = create_test_chore_manual(&db, product.id.clone(), format!("Chore {label}"));
    (db, product.id, chore.id)
}

/// Insert a pending conflict-resolution attempt for `work_item_id` with a
/// distinct `base_sha` (the `UNIQUE (work_item_id, base_sha_at_trigger)`
/// key, so each call lands its own row). Returns the inserted attempt.
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

/// Pin an attempt's `created_at` so ordering assertions are deterministic
/// rather than dependent on wall-clock resolution. Equal-length numeric
/// strings preserve `ORDER BY created_at DESC` as numeric order.
fn set_created_at(db: &WorkDb, attempt_id: &str, created_at: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE conflict_resolutions SET created_at = ?2 WHERE id = ?1",
            params![attempt_id, created_at],
        )
        .unwrap();
}

// ── list_conflict_resolutions ───────────────────────────────────────────

/// Rows come back freshest-first (`created_at DESC`) and `limit` caps the
/// result to the newest N.
#[test]
fn list_returns_freshest_first_and_respects_limit() {
    let (db, product, chore) = seed_product_and_chore("list-order");
    let a = insert_attempt(&db, &product, &chore, "sha-a");
    let b = insert_attempt(&db, &product, &chore, "sha-b");
    let c = insert_attempt(&db, &product, &chore, "sha-c");
    // Oldest → newest is a, b, c.
    set_created_at(&db, &a.id, "1700000001");
    set_created_at(&db, &b.id, "1700000002");
    set_created_at(&db, &c.id, "1700000003");

    let all = db.list_conflict_resolutions(None, &[], Some(&chore), None).unwrap();
    let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![c.id.as_str(), b.id.as_str(), a.id.as_str()],
        "list must return attempts newest-first"
    );

    let capped = db.list_conflict_resolutions(None, &[], Some(&chore), Some(2)).unwrap();
    let capped_ids: Vec<&str> = capped.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        capped_ids,
        vec![c.id.as_str(), b.id.as_str()],
        "limit must keep the newest N and drop the rest"
    );
}

/// The product and work-item filters are AND-ed and each restrict the
/// result to matching rows.
#[test]
fn list_filters_by_product_and_work_item() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let p1 = create_test_product_with_repo(&db, "list-p1", Some("git@example.invalid:foo/one.git"));
    let p2 = create_test_product_with_repo(&db, "list-p2", Some("git@example.invalid:foo/two.git"));
    let c1 = create_test_chore_manual(&db, p1.id.clone(), "c1");
    let c2 = create_test_chore_manual(&db, p2.id.clone(), "c2");
    let a1 = insert_attempt(&db, &p1.id, &c1.id, "p1-sha");
    let a2 = insert_attempt(&db, &p2.id, &c2.id, "p2-sha");

    // No filters → every row.
    assert_eq!(db.list_conflict_resolutions(None, &[], None, None).unwrap().len(), 2);

    // product filter isolates p1's row.
    let by_product = db.list_conflict_resolutions(Some(&p1.id), &[], None, None).unwrap();
    assert_eq!(by_product.len(), 1);
    assert_eq!(by_product[0].id, a1.id);

    // work-item filter isolates c2's row.
    let by_item = db.list_conflict_resolutions(None, &[], Some(&c2.id), None).unwrap();
    assert_eq!(by_item.len(), 1);
    assert_eq!(by_item[0].id, a2.id);

    // Contradictory AND (p1 product, c2 item) → no rows.
    assert!(
        db.list_conflict_resolutions(Some(&p1.id), &[], Some(&c2.id), None)
            .unwrap()
            .is_empty()
    );
}

/// An empty `statuses` slice means "any status"; a non-empty slice
/// restricts to the named statuses (OR-ed inside the `IN` clause).
#[test]
fn list_filters_by_status() {
    let (db, product, chore) = seed_product_and_chore("list-status");
    let done = insert_attempt(&db, &product, &chore, "sha-done");
    let bad = insert_attempt(&db, &product, &chore, "sha-bad");
    let pending = insert_attempt(&db, &product, &chore, "sha-pending");
    db.mark_conflict_resolution_succeeded(&done.id, None).unwrap();
    db.mark_conflict_resolution_failed(&bad.id, "gave_up").unwrap();

    // Empty slice → all three regardless of status.
    assert_eq!(
        db.list_conflict_resolutions(None, &[], Some(&chore), None)
            .unwrap()
            .len(),
        3,
        "an empty status slice must not filter"
    );

    // Single status.
    let succeeded = db
        .list_conflict_resolutions(None, &["succeeded".to_string()], Some(&chore), None)
        .unwrap();
    assert_eq!(succeeded.len(), 1);
    assert_eq!(succeeded[0].id, done.id);

    // Multiple statuses are OR-ed.
    let terminal = db
        .list_conflict_resolutions(
            None,
            &["succeeded".to_string(), "failed".to_string()],
            Some(&chore),
            None,
        )
        .unwrap();
    let mut terminal_ids: Vec<&str> = terminal.iter().map(|r| r.id.as_str()).collect();
    terminal_ids.sort_unstable();
    let mut want = vec![done.id.as_str(), bad.id.as_str()];
    want.sort_unstable();
    assert_eq!(terminal_ids, want);
    assert!(
        !terminal.iter().any(|r| r.id == pending.id),
        "the pending row must be excluded by the terminal-status filter"
    );
}

// ── latest_conflict_resolution_for_work_item ────────────────────────────

/// `None` when the work item has never had an attempt.
#[test]
fn latest_is_none_when_no_attempt_exists() {
    let (db, _product, chore) = seed_product_and_chore("latest-none");
    assert!(db.latest_conflict_resolution_for_work_item(&chore).unwrap().is_none());
}

/// Returns the single most recent attempt (by `created_at`) regardless of
/// its status, and is isolated to the given work item.
#[test]
fn latest_returns_most_recent_regardless_of_status() {
    let (db, product, chore) = seed_product_and_chore("latest-recent");
    let a = insert_attempt(&db, &product, &chore, "sha-old");
    let b = insert_attempt(&db, &product, &chore, "sha-mid");
    let c = insert_attempt(&db, &product, &chore, "sha-new");
    set_created_at(&db, &a.id, "1700000001");
    set_created_at(&db, &b.id, "1700000002");
    set_created_at(&db, &c.id, "1700000003");

    // Newest wins even though it is still pending and older ones are terminal.
    db.mark_conflict_resolution_succeeded(&a.id, None).unwrap();
    db.mark_conflict_resolution_failed(&b.id, "gave_up").unwrap();
    let latest = db
        .latest_conflict_resolution_for_work_item(&chore)
        .unwrap()
        .expect("an attempt exists");
    assert_eq!(latest.id, c.id);
    assert_eq!(latest.status, "pending");

    // Marking the newest terminal does not change which row is "latest".
    db.mark_conflict_resolution_succeeded(&c.id, None).unwrap();
    let after = db.latest_conflict_resolution_for_work_item(&chore).unwrap().unwrap();
    assert_eq!(after.id, c.id);
    assert_eq!(after.status, "succeeded");

    // A different work item's attempts are not returned.
    let other = create_test_chore_manual(&db, product.clone(), "other");
    assert!(
        db.latest_conflict_resolution_for_work_item(&other.id)
            .unwrap()
            .is_none()
    );
}

// ── has_active_rebase_attempt_for_pr ────────────────────────────────────

/// Create the dormant `rebase_attempts` side table (it ships with the
/// auto-rebase flow, not this one) and seed a row for `pr_url`.
fn create_rebase_attempt(db: &WorkDb, id: &str, pr_url: &str, status: &str) {
    let conn = db.connect().unwrap();
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
        "INSERT INTO rebase_attempts (id, dependent_pr_url, status) VALUES (?1, ?2, ?3)",
        params![id, pr_url, status],
    )
    .unwrap();
}

fn set_rebase_status(db: &WorkDb, id: &str, status: &str) {
    db.connect()
        .unwrap()
        .execute(
            "UPDATE rebase_attempts SET status = ?2 WHERE id = ?1",
            params![id, status],
        )
        .unwrap();
}

/// With no `rebase_attempts` table present, the method short-circuits to
/// `false` so the dispatch site reads identically before auto-rebase lands.
#[test]
fn has_active_rebase_is_false_without_side_table() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    assert!(
        !db.has_active_rebase_attempt_for_pr("https://github.com/foo/bar/pull/1")
            .unwrap()
    );
}

/// True only while the covering row is in a non-terminal status
/// (`pending`/`running`/`escalated`); false once it reaches a terminal one.
#[test]
fn has_active_rebase_tracks_non_terminal_status() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let pr = "https://github.com/foo/bar/pull/9";
    create_rebase_attempt(&db, "reb_1", pr, "running");
    assert!(db.has_active_rebase_attempt_for_pr(pr).unwrap(), "running → active");

    for active in ["pending", "escalated"] {
        set_rebase_status(&db, "reb_1", active);
        assert!(
            db.has_active_rebase_attempt_for_pr(pr).unwrap(),
            "{active} must count as active"
        );
    }

    for terminal in ["succeeded", "failed", "abandoned"] {
        set_rebase_status(&db, "reb_1", terminal);
        assert!(
            !db.has_active_rebase_attempt_for_pr(pr).unwrap(),
            "{terminal} must not count as active"
        );
    }
}

/// An active attempt for one PR does not make an unrelated PR read active.
#[test]
fn has_active_rebase_is_per_pr() {
    let db = WorkDb::open(PathBuf::from(":memory:")).unwrap();
    let pr_a = "https://github.com/foo/bar/pull/10";
    let pr_b = "https://github.com/foo/bar/pull/11";
    create_rebase_attempt(&db, "reb_a", pr_a, "running");
    assert!(db.has_active_rebase_attempt_for_pr(pr_a).unwrap());
    assert!(
        !db.has_active_rebase_attempt_for_pr(pr_b).unwrap(),
        "a rebase attempt for pr_a must not activate pr_b"
    );
}

// ── product_auto_pr_maintenance_enabled ─────────────────────────────────

/// Defaults to enabled: a freshly-created product has the column at its
/// `1` default, and a missing product id also reads enabled (the opt-out
/// only bites when explicitly set).
#[test]
fn auto_pr_maintenance_defaults_to_enabled() {
    let (db, product, _chore) = seed_product_and_chore("apm-default");
    assert!(
        db.product_auto_pr_maintenance_enabled(&product).unwrap(),
        "a new product must default to auto-maintenance enabled"
    );
    assert!(
        db.product_auto_pr_maintenance_enabled("prd_does_not_exist").unwrap(),
        "a missing product must read as enabled"
    );
}

/// Reflects an explicit toggle in both directions.
#[test]
fn auto_pr_maintenance_reflects_explicit_toggle() {
    let (db, product, _chore) = seed_product_and_chore("apm-toggle");

    db.connect()
        .unwrap()
        .execute(
            "UPDATE products SET auto_pr_maintenance_enabled = 0 WHERE id = ?1",
            params![product],
        )
        .unwrap();
    assert!(
        !db.product_auto_pr_maintenance_enabled(&product).unwrap(),
        "disabling the flag must read as false"
    );

    db.connect()
        .unwrap()
        .execute(
            "UPDATE products SET auto_pr_maintenance_enabled = 1 WHERE id = ?1",
            params![product],
        )
        .unwrap();
    assert!(
        db.product_auto_pr_maintenance_enabled(&product).unwrap(),
        "re-enabling the flag must read as true"
    );
}
