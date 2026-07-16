use super::*;

// ── Attention item integration tests (chore 16) ───────────────────────────

fn attention_items_for_product(db: &WorkDb, product_id: &str) -> Vec<boss_protocol::WorkAttentionItem> {
    db.list_attention_items_for_work_item(product_id)
        .expect("list attention items")
}

fn attention_items_for_work_item(db: &WorkDb, work_item_id: &str) -> Vec<boss_protocol::WorkAttentionItem> {
    db.list_attention_items_for_work_item(work_item_id)
        .expect("list attention items")
}

/// Reason 1: auth failure on `fetch_items` emits `external_tracker_auth_failed`
/// on the product.
#[tokio::test]
async fn attention_item_emitted_for_auth_failure() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![]);
    tracker.push_fetch_auth_error();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    let items = attention_items_for_product(&db, &product.id);
    let auth_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == "external_tracker_auth_failed" && i.status == "open")
        .collect();
    assert_eq!(
        auth_items.len(),
        1,
        "should emit exactly one auth_failed attention item"
    );
    assert!(
        auth_items[0].body_markdown.contains("gh auth login") || auth_items[0].body_markdown.contains("org approval"),
        "body should contain remediation hint; got: {}",
        auth_items[0].body_markdown
    );
}

/// 401 fetch error (token revoked) emits `external_tracker_token_revoked` on the product.
#[tokio::test]
async fn attention_item_emitted_for_token_revoked() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![]);
    tracker.push_fetch_token_revoked_error();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    let items = attention_items_for_product(&db, &product.id);
    let revoked_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == "external_tracker_token_revoked" && i.status == "open")
        .collect();
    assert_eq!(
        revoked_items.len(),
        1,
        "should emit exactly one token_revoked attention item"
    );
    assert!(
        revoked_items[0].body_markdown.contains("401") || revoked_items[0].body_markdown.contains("revoked"),
        "body should mention token revocation; got: {}",
        revoked_items[0].body_markdown
    );
}

/// Reason 2: transient fetch error emits `external_tracker_transient_errors`
/// on the product.
#[tokio::test]
async fn attention_item_emitted_for_transient_fetch_error() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![]);
    tracker.push_fetch_transient_error();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    let items = attention_items_for_product(&db, &product.id);
    let transient_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == "external_tracker_transient_errors" && i.status == "open")
        .collect();
    assert_eq!(
        transient_items.len(),
        1,
        "should emit exactly one transient_errors attention item"
    );
}

/// Reason 3: upstream item removed from project emits
/// `external_tracker_removed_upstream` on the unbound work item.
#[tokio::test]
async fn attention_item_emitted_for_removed_upstream() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore to be unbound");
    db.set_external_ref(&chore.id, "spy", "spy#20", &json!({ "issue_number": 20 }))
        .expect("set_external_ref");

    // Empty upstream: spy#20 is no longer in scope.
    let tracker = SpyTracker::new(vec![]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    let items = attention_items_for_work_item(&db, &chore.id);
    let unbound_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == "external_tracker_removed_upstream" && i.status == "open")
        .collect();
    assert_eq!(
        unbound_items.len(),
        1,
        "should emit exactly one removed_upstream attention item on the work item"
    );
    assert!(
        unbound_items[0].body_markdown.contains("spy#20"),
        "body should reference the canonical_id; got: {}",
        unbound_items[0].body_markdown
    );
}

/// Reason 4: `close_issue` permission denied emits
/// `external_tracker_permission_denied` on the work item.
#[tokio::test]
async fn attention_item_emitted_for_permission_denied_on_close() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = create_test_chore_manual(&db, product.id.clone(), "Chore with permission-denied close");
    db.set_external_ref(&chore.id, "spy", "spy#21", &json!({ "issue_number": 21 }))
        .expect("set_external_ref");

    // Upstream shows a merged PR; boss row is not yet done → close_issue fires.
    let tracker = SpyTracker::new(vec![item_with_merged_pr(
        21,
        "https://github.com/example/repo/pull/300",
    )]);
    tracker.push_permission_denied();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    let items = attention_items_for_work_item(&db, &chore.id);
    let perm_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == "external_tracker_permission_denied" && i.status == "open")
        .collect();
    assert_eq!(
        perm_items.len(),
        1,
        "should emit exactly one permission_denied attention item"
    );
    assert!(
        perm_items[0].body_markdown.contains("issues:write"),
        "body should mention required scope; got: {}",
        perm_items[0].body_markdown
    );
}

/// Idempotency: a second pass with the same auth failure does not create
/// a duplicate attention item.
#[tokio::test]
async fn attention_items_are_idempotent_on_repeated_failures() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![]);
    tracker.push_fetch_auth_error();
    tracker.push_fetch_auth_error();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    let items = attention_items_for_product(&db, &product.id);
    let auth_items: Vec<_> = items
        .iter()
        .filter(|i| i.kind == "external_tracker_auth_failed" && i.status == "open")
        .collect();
    assert_eq!(
        auth_items.len(),
        1,
        "repeated auth failures must not pile up duplicate attention items"
    );
}

/// Recovery: a successful fetch clears stale fetch-failure attention items.
#[tokio::test]
async fn attention_items_cleared_on_recovery() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![]);
    tracker.push_fetch_auth_error();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    // Tick 1: auth failure → attention item created.
    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    let items = attention_items_for_product(&db, &product.id);
    assert!(
        items
            .iter()
            .any(|i| i.kind == "external_tracker_auth_failed" && i.status == "open"),
        "attention item should exist after auth failure"
    );

    // Tick 2: fetch succeeds (no more queued error) → attention item resolved.
    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    let items2 = attention_items_for_product(&db, &product.id);
    let still_open = items2
        .iter()
        .filter(|i| i.kind == "external_tracker_auth_failed" && i.status == "open")
        .count();
    assert_eq!(
        still_open, 0,
        "auth_failed attention item should be resolved after recovery"
    );
}
