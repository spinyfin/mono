use super::*;

// ── Behavior 6: set project status to "In progress" ──────────────────────

fn seed_active_chore(db: &WorkDb, product_id: &str, canonical_id: &str, issue_num: u64) -> boss_protocol::Task {
    let chore = create_test_chore_manual(db, product_id, format!("Active chore {canonical_id}"));
    db.set_external_ref(&chore.id, "spy", canonical_id, &json!({ "issue_number": issue_num }))
        .expect("set_external_ref");
    // Simulate the task being dragged to Doing (active) via direct SQL,
    // mirroring what the engine's update_task RPC does.
    let conn = db.connect().expect("connect for seed_active_chore");
    conn.execute(
        "UPDATE tasks SET status = 'active' WHERE id = ?1",
        rusqlite::params![chore.id],
    )
    .expect("set status to active");
    db.find_by_external_ref("spy", canonical_id)
        .expect("query ok")
        .expect("chore exists")
}

/// Behavior 6 happy path: boss is active, upstream is Open, project_status
/// is "Todo" → set_project_status fires.
#[tokio::test]
async fn set_project_status_fires_when_boss_active_and_upstream_open_todo() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = seed_active_chore(&db, &product.id, "spy#30", 30);
    assert_eq!(chore.status, TaskStatus::Active);

    // Upstream is Open with project_status = "Todo" (not yet In progress).
    let item = open_item_with_project_status(30, "Issue 30", "Todo");
    let tracker = SpyTracker::new(vec![item]);
    tracker.push_set_project_status_ok();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.in_progress_set_succeeded, 1, "Behavior 6 should succeed");
    assert_eq!(outcome.in_progress_set_failed, 0);

    let calls = tracker.set_project_status_calls();
    assert_eq!(calls, vec!["spy#30"], "set_project_status called for spy#30");
}

/// Behavior 6 is idempotent: if the upstream item is already "In Progress",
/// set_project_status must NOT be called.
#[tokio::test]
async fn set_project_status_skipped_when_already_in_progress() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = seed_active_chore(&db, &product.id, "spy#31", 31);
    assert_eq!(chore.status, TaskStatus::Active);

    // Upstream already at "In Progress".
    let item = open_item_with_project_status(31, "Issue 31", "In Progress");
    let tracker = SpyTracker::new(vec![item]);
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(
        outcome.in_progress_set_succeeded, 0,
        "should not fire when already In progress"
    );
    assert_eq!(outcome.in_progress_set_failed, 0);
    assert!(
        tracker.set_project_status_calls().is_empty(),
        "set_project_status must not be called when already at target"
    );
}

/// Behavior 6 does not fire when the Boss task is in todo or done.
#[tokio::test]
async fn set_project_status_not_fired_for_non_active_task() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    // todo task
    let chore = create_test_chore_manual(&db, product.id.clone(), "Todo chore");
    db.set_external_ref(&chore.id, "spy", "spy#32", &json!({ "issue_number": 32 }))
        .expect("set_external_ref");
    // Leave as todo (default).

    let item = open_item_with_project_status(32, "Issue 32", "Todo");
    let tracker = SpyTracker::new(vec![item]);
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert_eq!(outcome.in_progress_set_succeeded, 0, "should not fire for todo task");
    assert!(tracker.set_project_status_calls().is_empty());
}

/// Behavior 6 does not fire when upstream is Closed (Behavior 2 handles that).
#[tokio::test]
async fn set_project_status_not_fired_when_upstream_closed() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = seed_active_chore(&db, &product.id, "spy#33", 33);
    assert_eq!(chore.status, TaskStatus::Active);

    // Upstream is Closed → Behavior 2 fires; Behavior 6 should not.
    let item = UpstreamItem {
        status: UpstreamStatus::Closed {
            reason: crate::external_tracker::ClosedReason::Completed,
        },
        project_status: Some("Done".to_owned()),
        ..open_item(33, "Closed issue 33")
    };
    let tracker = SpyTracker::new(vec![item]);
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;

    assert!(
        tracker.set_project_status_calls().is_empty(),
        "set_project_status must not be called when upstream is Closed"
    );
}

/// Behavior 6 transient failure: set_project_status is retried on the next tick.
#[tokio::test]
async fn set_project_status_transient_failure_retried_on_next_tick() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = seed_active_chore(&db, &product.id, "spy#34", 34);
    assert_eq!(chore.status, TaskStatus::Active);

    // project_status remains "Todo" both ticks (mutation didn't land yet).
    let item = open_item_with_project_status(34, "Issue 34", "Todo");
    let tracker = SpyTracker::new(vec![item]);

    // Tick 1: set_project_status fails transiently.
    tracker.push_set_project_status_transient();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome1 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome1.in_progress_set_failed, 1, "tick 1: should record failure");
    assert_eq!(outcome1.in_progress_set_succeeded, 0);

    // Tick 2: succeeds.
    tracker.push_set_project_status_ok();
    let outcome2 = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(outcome2.in_progress_set_succeeded, 1, "tick 2: should succeed");

    let calls = tracker.set_project_status_calls();
    assert_eq!(calls, vec!["spy#34", "spy#34"], "called on both ticks");
}

/// Behavior 6 fires when project_status is None (item has no Status column set yet).
#[tokio::test]
async fn set_project_status_fires_when_project_status_none() {
    let db = in_memory_db();
    let product = setup_product_with_tracker(&db);

    let chore = seed_active_chore(&db, &product.id, "spy#35", 35);
    assert_eq!(chore.status, TaskStatus::Active);

    // project_status is None (Status field not set on the GitHub Project item).
    let item = open_item(35, "Issue 35");
    assert!(item.project_status.is_none());
    let tracker = SpyTracker::new(vec![item]);
    tracker.push_set_project_status_ok();
    let registry = spy_registry(Arc::clone(&tracker));
    let metrics = Registry::new();
    register_metrics(&metrics);

    let outcome = run_one_pass(&db, &registry, &metrics, &noop_pub(), &ambient_resolver()).await;
    assert_eq!(
        outcome.in_progress_set_succeeded, 1,
        "should fire when project_status is None"
    );
    let calls = tracker.set_project_status_calls();
    assert_eq!(calls, vec!["spy#35"]);
}
