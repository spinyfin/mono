use super::*;

// ── Behavior 3 (reverse-close) tests ─────────────────────────────────────

// Helper: create a chore that is already in `done` status.
fn seed_done_chore(db: &WorkDb, product_id: &str, canonical_id: &str, issue_num: u64) -> boss_protocol::Task {
    let chore = create_test_chore_manual(db, product_id, format!("Done chore {canonical_id}"));
    db.set_external_ref(&chore.id, "spy", canonical_id, &json!({ "issue_number": issue_num }))
        .expect("set_external_ref");
    db.reconciler_close_work_item(&chore.id).expect("close work item");
    db.find_by_external_ref("spy", canonical_id)
        .expect("query ok")
        .expect("chore exists")
}

/// Behavior 3 happy path: `reverse_close=true`, boss is done, no merged PR
/// → `close_issue` fires.
#[tokio::test]
async fn reverse_close_fires_when_boss_done_and_no_merged_pr() {
    let db = in_memory_db();
    let product = setup_product_with_reverse_close(&db);

    let chore = seed_done_chore(&db, &product.id, "spy#10", 10);
    assert_eq!(chore.status, TaskStatus::Done);

    // Upstream still Open, no PR associations.
    let tracker = SpyTracker::new(vec![open_item(10, "Issue 10")]);
    tracker.push_ok();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.reverse_close_succeeded, 1, "reverse-close should succeed");
    assert_eq!(outcome.close_issue_succeeded, 0, "Behavior 5 should NOT fire");

    let calls = tracker.close_calls();
    assert_eq!(calls, vec!["spy#10"], "close_issue should be called once");
}

/// Behavior 3 + Behavior 5 mutual exclusion: `reverse_close=true` AND
/// upstream shows a merged PR → Behavior 5 fires; reverse-close is skipped.
#[tokio::test]
async fn reverse_close_skipped_when_pr_merge_drove_transition() {
    let db = in_memory_db();
    let product = setup_product_with_reverse_close(&db);

    // Boss row bound and done, upstream shows a merged PR.
    let chore = seed_done_chore(&db, &product.id, "spy#11", 11);
    assert_eq!(chore.status, TaskStatus::Done);

    let tracker = SpyTracker::new(vec![item_with_merged_pr(
        11,
        "https://github.com/example/repo/pull/200",
    )]);
    tracker.push_ok();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    // Behavior 5 fires; reverse-close path must not.
    assert_eq!(outcome.close_issue_succeeded, 1, "Behavior 5 should succeed");
    assert_eq!(outcome.reverse_close_succeeded, 0, "Behavior 3 must not fire");
    assert_eq!(outcome.reverse_close_failed, 0);

    let calls = tracker.close_calls();
    assert_eq!(calls.len(), 1, "close_issue called exactly once");
    assert_eq!(calls[0], "spy#11");
}

/// With `reverse_close=false` (the default), boss done without a merged PR
/// → no `close_issue` call regardless.
#[tokio::test]
async fn reverse_close_disabled_by_default_no_close_issue() {
    let db = in_memory_db();
    // Default product has reverse_close absent (defaults to false).
    let product = setup_product_with_tracker(&db);

    let chore = seed_done_chore(&db, &product.id, "spy#12", 12);
    assert_eq!(chore.status, TaskStatus::Done);

    // Upstream still Open, no PR associations.
    let tracker = SpyTracker::new(vec![open_item(12, "Issue 12")]);
    // No close response queued — if close_issue is called, it returns Ok(())
    // via the default, but we still assert it wasn't called.
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.reverse_close_succeeded, 0, "reverse-close disabled");
    assert_eq!(outcome.reverse_close_failed, 0);
    assert_eq!(outcome.close_issue_succeeded, 0, "Behavior 5 should not fire either");

    let calls = tracker.close_calls();
    assert!(
        calls.is_empty(),
        "close_issue must not be called when reverse_close=false"
    );
}

// ── E2E: import → done → reverse_close ───────────────────────────────────

/// Verify the full import→done→reverse_close path using `run_one_pass` for
/// the import (rather than the `seed_done_chore` helper that bypasses
/// `import_chore_with_external_ref`).  This guards against regressions
/// where the importer creates the chore and the external_ref binding
/// non-atomically, leaving the chore invisible to the reconciler.
#[tokio::test]
async fn reverse_close_fires_for_reconciler_imported_chore() {
    let db = in_memory_db();
    let _product = setup_product_with_reverse_close(&db);

    // Pass 1: import the upstream item as a new Boss chore.
    let tracker = SpyTracker::new(vec![open_item(30, "Issue 30")]);
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome1.items_imported, 1, "pass 1: should import one item");

    // The imported chore must have its external_ref bound so the
    // reconciler can reach it on subsequent passes.
    let chore = db
        .find_by_external_ref("spy", "spy#30")
        .expect("query ok")
        .expect("imported chore must be findable by external_ref");
    assert_eq!(chore.status, TaskStatus::Todo);

    // Simulate the chore being completed (e.g. PR merged → boss dragged to done).
    db.reconciler_close_work_item(&chore.id).expect("close work item");
    let closed = db
        .find_by_external_ref("spy", "spy#30")
        .expect("query ok")
        .expect("chore still exists");
    assert_eq!(closed.status, TaskStatus::Done);

    // Pass 2: upstream still Open, boss is done, no merged PR
    //         → reverse_close must fire.
    tracker.push_ok();
    let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(
        outcome2.reverse_close_succeeded, 1,
        "reverse_close must fire for an imported-then-done chore"
    );
    assert_eq!(outcome2.items_imported, 0, "no duplicate import must occur");

    let calls = tracker.close_calls();
    assert_eq!(calls, vec!["spy#30"], "close_issue called exactly once");
}
