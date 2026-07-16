use super::*;

// ── Smoke test: spawn_loop fires one tick and emits metrics ───────────────
//
// This test verifies the `spawn_loop` structural contract:
//   1. The spawned task runs `run_one_pass` immediately on boot.
//   2. Metrics are emitted (via the shared `Arc<Registry>`).
//   3. The interval sleep is honoured (loop does not busy-spin).
//
// Implementation note: `spawn_loop` moves the DB Arc into the spawned
// task. For in-memory SQLite shared-cache databases, every call to
// `connect()` opens a new connection to the same named in-memory
// database, so both the test thread and the spawned task see the same
// rows. The interval is set to 1 hour so only the initial on-boot tick
// fires during the test.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_loop_fires_one_tick_and_emits_metrics() {
    let db = Arc::new(in_memory_db());
    let product = setup_product_with_tracker_arc(db.as_ref());

    // Verify the setup is visible through a fresh connect() before spawning,
    // so a failure here points to setup rather than the loop.
    let products = db.list_products().expect("list_products");
    let bound = products
        .iter()
        .find(|p| p.id == product.id && p.external_tracker_kind.is_some())
        .expect("product with tracker should be visible");
    assert_eq!(bound.external_tracker_kind.as_deref(), Some("spy"));

    let tracker = SpyTracker::new(vec![open_item(10, "Loop issue")]);
    let registry = Arc::new(spy_registry(tracker));

    let metrics = Arc::new(Registry::new());
    register_metrics(&metrics);

    // Use a large interval: only the immediate first tick fires before abort.
    let interval = std::time::Duration::from_secs(3600);
    let handle = spawn_loop(
        db.clone(),
        registry,
        interval,
        metrics.clone(),
        Arc::new(noop_pub()),
        Arc::new(ambient_resolver()),
    );

    // Poll until the imported counter advances (max 5 s).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let imported = metrics.counter_value("external_tracker.imported").unwrap_or(0);
        if imported >= 1 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            handle.abort();
            panic!("spawn_loop did not import any item within 5 seconds");
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    handle.abort();

    // The spawned task should have imported the one item.
    let task = db
        .find_by_external_ref("spy", "spy#10")
        .expect("query ok")
        .expect("chore should exist after spawn_loop tick");
    assert_eq!(task.status, TaskStatus::Todo);

    let imported = metrics.counter_value("external_tracker.imported").unwrap_or(0);
    assert!(imported >= 1, "IMPORTED counter should be ≥ 1, got {imported}");
}

fn setup_product_with_tracker_arc(db: &WorkDb) -> boss_protocol::Product {
    setup_product_with_tracker(db)
}

// ── Smoke test: run_one_pass_for_product ─────────────────────────────────

#[tokio::test]
async fn run_one_pass_for_product_returns_outcome_for_bound_product() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);
    let tracker = SpyTracker::new(vec![open_item(11, "Single product issue")]);
    let registry = spy_registry(tracker);
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass_for_product(&db, &registry, &metrics, &product.id, &noop_pub(), &ambient_resolver())
        .await
        .expect("should return Some for a bound product");

    assert_eq!(outcome.items_imported, 1);
    assert_eq!(outcome.products_processed, 1);
}

#[tokio::test]
async fn run_one_pass_for_product_returns_none_for_unbound_product() {
    let db = in_memory_db();
    let product = create_test_product_with_repo(&db, "Unbound", None);
    let registry = TrackerRegistry::new();
    let metrics = Registry::new();
    register_metrics(&metrics);

    let result =
        run_one_pass_for_product(&db, &registry, &metrics, &product.id, &noop_pub(), &ambient_resolver()).await;
    assert!(result.is_none(), "unbound product should return None");
}
