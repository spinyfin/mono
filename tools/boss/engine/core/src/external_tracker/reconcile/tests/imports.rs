use super::*;

// ── Test: create (new upstream → new boss row) ────────────────────────────

#[tokio::test]
async fn create_imports_new_open_upstream_item() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![open_item(1, "My issue")]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.items_imported, 1, "should import one item");
    assert_eq!(outcome.products_processed, 1);

    let task = db
        .find_by_external_ref("spy", "spy#1")
        .expect("query ok")
        .expect("chore should exist");
    assert_eq!(task.status, TaskStatus::Todo);
    assert!(task.name.contains("My issue"), "name should come from title");
    assert_eq!(task.product_id, product.id);
    let ext = task.external_ref.expect("external_ref should be set");
    assert_eq!(ext.canonical_id, "spy#1");
    assert!(ext.synced_at.is_some(), "synced_at should be set after import");
}

// ── Test: import emits work-invalidation event ────────────────────────────

#[tokio::test]
async fn import_emits_chore_created_invalidation() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![open_item(99, "Event test issue")]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);
    let publisher = Arc::new(RecordingPublisher::default());

    run_one_pass(&db, &registry, &metrics, publisher.as_ref(), &ambient_resolver()).await;

    let calls = publisher.recorded();
    assert_eq!(calls.len(), 1, "expected exactly one invalidation event");
    let (pid, _wid, reason) = &calls[0];
    assert_eq!(pid, &product.id, "product_id should match");
    assert_eq!(reason, "chore_created", "reason should be chore_created");
}

// ── Test: tracked label attach (Behavior 7) ───────────────────────────────

#[tokio::test]
async fn import_attaches_tracked_label_to_upstream() {
    let db = in_memory_db();
    setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![open_item(50, "Fresh issue")]);
    let registry = spy_registry(tracker.clone());
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.items_imported, 1);
    assert_eq!(outcome.tracked_label_attach_succeeded, 1);
    assert_eq!(outcome.tracked_label_attach_failed, 0);
    let calls = tracker.add_label_calls();
    assert_eq!(
        calls,
        vec![("spy#50".to_owned(), "tracked".to_owned())],
        "add_label should be called once with the tracked label"
    );
}

#[tokio::test]
async fn import_skips_add_label_when_already_present_upstream() {
    let db = in_memory_db();
    setup_product_with_tracker(&db);
    let mut item = open_item(51, "Already labelled issue");
    item.labels.push("tracked".to_owned());
    let tracker = SpyTracker::new(vec![item]);
    let registry = spy_registry(tracker.clone());
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.items_imported, 1);
    assert_eq!(
        outcome.tracked_label_attach_succeeded, 0,
        "should not count an attach when label already present"
    );
    assert!(
        tracker.add_label_calls().is_empty(),
        "add_label must not be called when 'tracked' is already in upstream.labels"
    );
}

#[tokio::test]
async fn import_succeeds_even_when_add_label_fails_transiently() {
    let db = in_memory_db();
    setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![open_item(52, "Unlucky issue")]);
    tracker.push_add_label_transient();
    let registry = spy_registry(tracker.clone());
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    // Import itself must still succeed.
    assert_eq!(outcome.items_imported, 1);
    assert_eq!(outcome.tracked_label_attach_succeeded, 0);
    assert_eq!(outcome.tracked_label_attach_failed, 1);
    let chore = db
        .find_by_external_ref("spy", "spy#52")
        .expect("query ok")
        .expect("chore should exist despite label failure");
    assert_eq!(chore.status, TaskStatus::Todo);
}

#[tokio::test]
async fn add_label_not_called_for_closed_at_first_sight() {
    let db = in_memory_db();
    setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![closed_item(53)]);
    let registry = spy_registry(tracker.clone());
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert!(
        tracker.add_label_calls().is_empty(),
        "add_label must not be called when an item is skipped at first sight"
    );
}
