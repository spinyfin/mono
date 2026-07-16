use super::*;

#[tokio::test]
async fn reconcile_retries_tracked_label_when_missing_on_existing_item() {
    // Behavior 7 retry: an already-imported item whose upstream fetch shows
    // no `tracked` label must trigger add_label on each reconcile pass until
    // the label is confirmed present. This handles the backfill case where
    // the initial import-time add_label failed.
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Already bound, label missing");
    db.set_external_ref(&chore.id, "spy", "spy#54", &json!({ "issue_number": 54 }))
        .expect("set_external_ref");

    // open_item has no labels — tracked label is missing upstream.
    let tracker = SpyTracker::new(vec![open_item(54, "Already bound issue")]);
    let registry = spy_registry(tracker.clone());
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(
        outcome.tracked_label_attach_succeeded, 1,
        "reconcile should attach tracked label when it is missing"
    );
    assert_eq!(
        tracker.add_label_calls(),
        vec![("spy#54".to_owned(), "tracked".to_owned())],
        "add_label must be called once with (canonical_id, 'tracked')"
    );
}

#[tokio::test]
async fn add_label_not_called_when_already_present_on_existing_item() {
    // If the upstream fetch shows `tracked` is already present, the
    // reconciler must not call add_label again (avoid redundant API calls).
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Already labeled");
    db.set_external_ref(&chore.id, "spy", "spy#55", &json!({ "issue_number": 55 }))
        .expect("set_external_ref");

    let mut item = open_item(55, "Already labeled issue");
    item.labels.push("tracked".to_owned());
    let tracker = SpyTracker::new(vec![item]);
    let registry = spy_registry(tracker.clone());
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert!(
        tracker.add_label_calls().is_empty(),
        "add_label must not be called when 'tracked' is already confirmed upstream"
    );
}

// ── Test: skip already-closed at first sight ──────────────────────────────

#[tokio::test]
async fn closed_at_first_sight_is_skipped() {
    let db = in_memory_db();
    setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![closed_item(2)]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.items_imported, 0, "closed item should be skipped");
    let found = db.find_by_external_ref("spy", "spy#2").expect("query ok");
    assert!(found.is_none(), "should not have imported a closed item");
}

// ── Test: close-mirror (Behavior 2) ──────────────────────────────────────

#[tokio::test]
async fn close_mirror_sets_boss_row_done_when_upstream_closes() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    // Seed a Boss chore bound to upstream#3.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Open chore");
    db.set_external_ref(&chore.id, "spy", "spy#3", &json!({ "issue_number": 3 }))
        .expect("set_external_ref");

    // Upstream now shows issue as closed.
    let tracker = SpyTracker::new(vec![closed_item(3)]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.items_closed, 1);
    let updated = db
        .find_by_external_ref("spy", "spy#3")
        .expect("query ok")
        .expect("chore should still exist");
    assert_eq!(updated.status, TaskStatus::Done, "boss row should be done");
}

// ── Test: pr-attach (Behavior 4) ─────────────────────────────────────────

#[tokio::test]
async fn pr_attach_writes_pr_url_when_upstream_has_pr_association() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore without PR");
    db.set_external_ref(&chore.id, "spy", "spy#4", &json!({ "issue_number": 4 }))
        .expect("set_external_ref");

    let mut item = open_item(4, "Issue with open PR");
    item.pr_associations = vec![UpstreamPrAssociation {
        pr_url: "https://github.com/example/repo/pull/99".to_owned(),
        merged: false,
        merged_at: None,
    }];

    let tracker = SpyTracker::new(vec![item]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.pr_attached, 1);
    let updated = db
        .find_by_external_ref("spy", "spy#4")
        .expect("query ok")
        .expect("chore exists");
    assert_eq!(
        updated.pr_url.as_deref(),
        Some("https://github.com/example/repo/pull/99")
    );
}

// ── Test: pr-merge-close (Behavior 5) ────────────────────────────────────

#[tokio::test]
async fn pr_merge_close_calls_close_issue_on_tracker_and_marks_boss_done() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore with merged PR");
    db.set_external_ref(&chore.id, "spy", "spy#5", &json!({ "issue_number": 5 }))
        .expect("set_external_ref");

    let tracker = SpyTracker::new(vec![item_with_merged_pr(5, "https://github.com/example/repo/pull/101")]);
    tracker.push_ok();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.close_issue_succeeded, 1, "close_issue should succeed");
    assert_eq!(outcome.items_closed, 1, "boss row should flip to done");

    let updated = db
        .find_by_external_ref("spy", "spy#5")
        .expect("query ok")
        .expect("chore exists");
    assert_eq!(updated.status, TaskStatus::Done);

    let calls = tracker.close_calls();
    assert_eq!(calls, vec!["spy#5"], "close_issue should have been called for spy#5");
}

// ── Test: unbind (removed from upstream) ─────────────────────────────────

#[tokio::test]
async fn unbind_clears_external_ref_when_upstream_item_disappears() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore that will be unbound");
    db.set_external_ref(&chore.id, "spy", "spy#6", &json!({ "issue_number": 6 }))
        .expect("set_external_ref");

    // Empty upstream: item #6 has been removed from the project.
    let tracker = SpyTracker::new(vec![]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.items_unbound, 1, "one item should be unbound");

    // The row should still exist but the ref should be unbound.
    let refs = db.list_external_refs_for_product(&product.id).expect("list ok");
    let (_, stored) = refs
        .iter()
        .find(|(_, r)| r.canonical_id == "spy#6")
        .expect("stored ref should still exist");
    assert!(stored.unbound_at.is_some(), "unbound_at should be set");
    assert!(stored.synced_at.is_none(), "synced_at should be cleared");
}

// ── Test: transient close failure → retry on next tick ───────────────────

#[tokio::test]
async fn transient_close_failure_is_retried_on_next_tick() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore for retry test");
    db.set_external_ref(&chore.id, "spy", "spy#7", &json!({ "issue_number": 7 }))
        .expect("set_external_ref");

    let upstream = item_with_merged_pr(7, "https://github.com/example/repo/pull/200");
    let tracker = SpyTracker::new(vec![upstream]);

    // Tick 1: close_issue fails transiently.
    tracker.push_transient();

    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome1.close_issue_failed, 1, "tick 1: should record failed close");
    assert_eq!(outcome1.items_closed, 1, "tick 1: boss row should flip to done");

    let after_tick1 = db
        .find_by_external_ref("spy", "spy#7")
        .expect("query ok")
        .expect("chore exists");
    assert_eq!(after_tick1.status, TaskStatus::Done, "boss row done after tick 1");

    // Tick 2: upstream is still Open (close didn't land); close_issue succeeds.
    tracker.push_ok();

    let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome2.close_issue_succeeded, 1, "tick 2: close should succeed");
    assert_eq!(outcome2.items_closed, 0, "tick 2: boss already done, no extra close");

    let calls = tracker.close_calls();
    // close_issue called once on tick 1 (failed) and once on tick 2 (succeeded).
    assert_eq!(calls, vec!["spy#7", "spy#7"], "close_issue called on both ticks");
}

// ── Test: idempotency — no changes when state is already consistent ───────

#[tokio::test]
async fn idempotent_when_already_synced() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let tracker = SpyTracker::new(vec![open_item(8, "Stable issue")]);
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    // First pass: import.
    let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome1.items_imported, 1);

    // Second pass: nothing should change.
    let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome2.items_imported, 0);
    assert_eq!(outcome2.items_closed, 0);
    assert_eq!(outcome2.pr_attached, 0);
    assert_eq!(outcome2.close_issue_succeeded, 0);

    // Boss row unchanged.
    let task = db
        .find_by_external_ref("spy", "spy#8")
        .expect("query ok")
        .expect("chore exists");
    assert_eq!(task.status, TaskStatus::Todo);
    assert!(task.pr_url.is_none());
    assert_eq!(task.product_id, product.id);
}

// ── Test: rebind when upstream item reappears after unbind ───────────────

#[tokio::test]
async fn rebind_when_upstream_item_reappears() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    // Seed a chore with external ref bound.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Rebind test chore");
    db.set_external_ref(&chore.id, "spy", "spy#9", &json!({ "issue_number": 9 }))
        .expect("set_external_ref");

    // Pass 1: upstream is empty → unbind.
    let tracker_empty = SpyTracker::new(vec![]);
    let registry_empty = spy_registry(tracker_empty);
    let metrics = Registry::new();
    register_metrics(&metrics);
    let outcome_unbind = run_one_pass(&db, &registry_empty, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome_unbind.items_unbound, 1);

    // Verify unbound.
    let refs = db.list_external_refs_for_product(&product.id).expect("list ok");
    let (_, stored) = refs.iter().find(|(_, r)| r.canonical_id == "spy#9").unwrap();
    assert!(stored.unbound_at.is_some(), "should be unbound");

    // Pass 2: item reappears upstream → should re-bind, not create a duplicate.
    let tracker_reappear = SpyTracker::new(vec![open_item(9, "Reappeared issue")]);
    let registry_reappear = spy_registry(tracker_reappear);
    let metrics2 = Registry::new();
    register_metrics(&metrics2);
    let outcome_rebind = run_one_pass(&db, &registry_reappear, &metrics2, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome_rebind.items_imported, 0, "should rebind, not import");

    // Only one chore with spy#9 should exist.
    let rows = db.list_external_refs_for_product(&product.id).expect("list ok");
    let spy9_rows: Vec<_> = rows.iter().filter(|(_, r)| r.canonical_id == "spy#9").collect();
    assert_eq!(spy9_rows.len(), 1, "exactly one binding for spy#9");
    let (_, bound) = spy9_rows[0];
    assert!(bound.unbound_at.is_none(), "should be rebound (unbound_at cleared)");
}
