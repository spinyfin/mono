use super::*;

#[tokio::test]
async fn attention_resolution_retries_after_claim_release_and_database_failure() {
    let (_dir, db) = open_db();
    let product = create_product(&db);
    let exec = create_execution(&db, &create_active_chore(&db, &product, "attention retry"));
    force_completed(&db, &exec);
    age_finished_at(&db, &exec, 300);
    let db = Arc::new(db);
    let coordinator = make_coordinator(db.clone(), 1);
    let pool = coordinator.worker_pool();
    pool.claim_worker(&exec, None).await.unwrap();
    let mut retries = TeardownRetries::default();
    for _ in 0..3 {
        retries.failed(&db, &exec, "unconfirmed");
    }
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TRIGGER refuse_attention_resolution BEFORE UPDATE ON work_attention_items
         BEGIN SELECT RAISE(ABORT, 'resolution unavailable'); END;",
        )
        .unwrap();
    let live = LiveWorkerStateRegistry::new();
    let sink = RecordingDispatchEventSink::new();
    let outcome = run_one_pass(&db, &live, coordinator.clone(), &sink, &NoViewers, &mut retries).await;
    assert_eq!(outcome.released, 1);
    assert!(pool.claims().await.is_empty());
    assert_eq!(
        db.list_open_attention_items_of_kind(TEARDOWN_ATTENTION_KIND)
            .unwrap()
            .len(),
        1
    );
    db.connect()
        .unwrap()
        .execute_batch("DROP TRIGGER refuse_attention_resolution")
        .unwrap();
    run_one_pass(&db, &live, coordinator, &sink, &NoViewers, &mut retries).await;
    assert!(
        db.list_open_attention_items_of_kind(TEARDOWN_ATTENTION_KIND)
            .unwrap()
            .is_empty()
    );
    assert!(retries.failures.is_empty());
}

#[tokio::test]
async fn attention_reconciles_external_release_and_restart_without_retry_state() {
    for restart in [false, true] {
        let (_dir, db) = open_db();
        let product = create_product(&db);
        let exec = create_execution(&db, &create_active_chore(&db, &product, "external release"));
        let db = Arc::new(db);
        let coordinator = make_coordinator(db.clone(), 1);
        let worker = coordinator.worker_pool().claim_worker(&exec, None).await.unwrap();
        let mut retries = TeardownRetries::default();
        for _ in 0..3 {
            retries.failed(&db, &exec, "unconfirmed");
        }
        let live = LiveWorkerStateRegistry::new();
        let sink = RecordingDispatchEventSink::new();
        run_one_pass(&db, &live, coordinator.clone(), &sink, &NoViewers, &mut retries).await;
        assert_eq!(
            db.list_open_attention_items_of_kind(TEARDOWN_ATTENTION_KIND)
                .unwrap()
                .len(),
            1
        );
        assert!(coordinator.release_pool_claim_if_execution(&worker, &exec).await);
        if restart {
            retries = TeardownRetries::default();
        }
        run_one_pass(&db, &live, coordinator, &sink, &NoViewers, &mut retries).await;
        assert!(
            db.list_open_attention_items_of_kind(TEARDOWN_ATTENTION_KIND)
                .unwrap()
                .is_empty()
        );
        assert!(retries.failures.is_empty());
    }
}
