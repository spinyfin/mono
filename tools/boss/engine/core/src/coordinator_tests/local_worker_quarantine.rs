use super::helpers::*;

#[test]
fn historical_quarantine_blocks_local_placement_and_duplicate_dispatch_but_allows_remote_work() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let historical = create_test_chore(&db, product.id.clone(), "Historical local worker");
    let execution = create_old_execution(&db, &historical.id);
    db.start_execution_run(&execution, "worker-1", "repo", "lease", "workspace", "/tmp/workspace")
        .unwrap();
    let report = crate::local_worker_quarantine::quarantine_historical_local_workers(&db).unwrap();
    assert!(report.protected_execution_ids.contains(&execution));

    db.add_host("remote", "user@remote", 1, &[]).unwrap();
    insert_host_capability(&db, "remote", "driver=claude", "auto");
    let unrelated = create_test_chore(&db, product.id, "Unrelated work");
    let coordinator = ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        Arc::new(FakeCubeClient::default()),
        Arc::new(FakeExecutionRunner::default()),
    );
    let error = coordinator.validate_requested_host(&unrelated.id, "local").unwrap_err();
    assert!(error.to_string().contains("local dispatch quarantined"), "{error:#}");
    coordinator.validate_requested_host(&unrelated.id, "remote").unwrap();
    assert!(coordinator.validate_requested_host(&historical.id, "remote").is_err());
    let error = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&historical.id).build())
        .unwrap_err();
    assert!(error.to_string().contains("quarantined"), "{error:#}");
    assert_eq!(db.list_executions(Some(&historical.id)).unwrap().len(), 1);
    assert!(!db.get_execution(&execution).unwrap().status.is_terminal());
}
