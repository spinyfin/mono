//! Backend identities and admission remain independent of viewer layout.
use super::helpers::*;

#[tokio::test]
async fn backend_pool_geometry_is_fixed() {
    assert_eq!(MAX_WORKER_POOL_SIZE, 16);
    assert_eq!(MAX_AUTOMATION_POOL_SIZE, 8);
    assert_eq!(MAX_REVIEW_POOL_SIZE, 16);
    assert_eq!(DEFAULT_REVIEW_POOL_SIZE, 16);
    assert_eq!(MAX_CONCURRENT_INTERACTIVE_WORKERS, 8);
    for (pool, prefix, first, last) in [
        (WorkerPool::new(100), "worker-", 1u8, 16u8),
        (WorkerPool::new_automation(100), "auto-worker-", 17, 24),
        (WorkerPool::new_review(100), "review-", 25, 40),
    ] {
        assert_eq!(pool.capacity().await, usize::from(last - first + 1));
        for slot in first..=last {
            let expected = format!("{prefix}{}", slot - first + 1);
            let worker = pool.claim_worker(&format!("exec-{slot}"), None).await.unwrap();
            assert_eq!(worker, expected);
            assert_eq!(slot_id_from_worker_id(&worker), Some(slot));
            assert_eq!(worker_id_for_slot(slot), worker);
        }
        assert!(pool.claim_worker("overflow", None).await.is_none());
    }
}

#[tokio::test]
async fn interactive_cap_only_holds_new_main_pool_admission() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    seed_local_claude_driver(&db);
    let (product_id, chore_id) = make_pr_review_fixture(&db, None);
    let review = db
        .create_execution(
            CreateExecutionInput::builder()
                .work_item_id(chore_id)
                .kind(ExecutionKind::PrReview)
                .status(ExecutionStatus::Ready)
                .build(),
        )
        .unwrap();
    let chore = create_test_chore(&db, product_id, "Held main work");
    db.reconcile_product_executions(&chore.product_id).unwrap();
    let main = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    let runner = Arc::new(FakeExecutionRunner {
        pending: true,
        ..Default::default()
    });
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(16),
        Arc::new(FakeCubeClient::default()),
        runner.clone(),
    ));
    assert_eq!(coordinator.max_concurrent_interactive_workers(), 8);
    for i in 0..8 {
        coordinator
            .worker_pool()
            .claim_worker(&format!("busy-{i}"), None)
            .await
            .unwrap();
    }
    coordinator.drain_ready_queue().await;
    wait_for_execution_status(&db, &review.id, ExecutionStatus::Running).await;
    assert_eq!(runner.calls.lock().await.len(), 1);
    assert!(db.active_run_ids_for_execution(&main.id).unwrap().is_empty());
    let held = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(held.status, ExecutionStatus::Ready);
    assert!(
        held.dispatch_wait_reason
            .unwrap()
            .contains("interactive concurrency cap")
    );
    // Lowering the cap changes admission, never the pool or existing claims.
    coordinator.set_max_concurrent_interactive_workers(4).unwrap();
    coordinator.drain_ready_queue().await;
    assert!(db.active_run_ids_for_execution(&main.id).unwrap().is_empty());
    assert_eq!(coordinator.worker_pool().capacity().await, 16);
    assert_eq!(coordinator.worker_pool().idle_count().await, 8);
    for (i, claim) in coordinator.worker_pool().claims().await.iter().enumerate() {
        assert_eq!(claim.worker_id, format!("worker-{}", i + 1));
        assert_eq!(claim.execution_id, format!("busy-{i}"));
    }
    assert_eq!(coordinator.review_worker_pool().idle_count().await, 15);
}
