//! Dispatch around a local worker the engine cannot verify: one such worker
//! holds its own work item and nothing else, a spawn that never reached tmux
//! holds nothing at all, and a hold lifts without an engine restart.

use super::helpers::*;
use crate::local_worker_quarantine::{SPAWN_NEVER_REACHED_TMUX_REASON, quarantine_historical_local_workers};
use crate::run_reconcile::RunReconcileVerdict;

fn coordinator_for(db: &Arc<WorkDb>, runner: Arc<FakeExecutionRunner>) -> Arc<ExecutionCoordinator> {
    Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(2),
        Arc::new(FakeCubeClient::default()),
        runner,
    ))
}

/// A running local execution from before tmux hosting: no hosting stamp, no
/// identity, no pid. Nothing can prove its worker dead.
fn seed_unverifiable_worker(db: &WorkDb, product_id: &str, name: &str) -> (String, String) {
    let chore = create_test_chore(db, product_id, name);
    let execution = create_old_execution(db, &chore.id);
    db.start_execution_run(&execution, "worker-1", "repo", "lease", "workspace", "/tmp/workspace")
        .unwrap();
    (execution, chore.id)
}

/// A running local execution as dispatch records it before the spawn reaches
/// tmux, with no identity ever arriving: the engine process stopped first.
fn seed_spawn_that_never_got_an_identity(db: &WorkDb, product_id: &str, name: &str) -> (String, String) {
    let chore = create_test_chore(db, product_id, name);
    let execution = create_old_execution(db, &chore.id);
    db.start_execution_run_on_host_with_tmux_hosting(
        &execution,
        "worker-1",
        "repo",
        "lease",
        "workspace",
        "/tmp/workspace",
        "local",
        true,
    )
    .unwrap();
    let execution_row = db.get_execution(&execution).unwrap();
    assert_eq!(execution_row.status, ExecutionStatus::Running);
    assert!(db.tmux_identity_for_execution(&execution).unwrap().is_none());
    assert_eq!(db.latest_local_shell_pid_for_execution(&execution).unwrap(), None);
    (execution, chore.id)
}

/// Queue `work_item_id` and wait for the coordinator to start its run on the
/// local host.
async fn dispatches_locally(db: &Arc<WorkDb>, coordinator: &Arc<ExecutionCoordinator>, work_item_id: &str) {
    let execution = db
        .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
        .unwrap();
    coordinator.kick();
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;
    assert_eq!(
        db.latest_run_host_for_execution(&execution.id).unwrap().as_deref(),
        Some("local")
    );
}

#[tokio::test]
async fn an_unverifiable_worker_holds_only_its_own_work_item() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let (execution, held_item) = seed_unverifiable_worker(&db, &product.id, "Worker from before tmux hosting");
    let report = quarantine_historical_local_workers(&db).unwrap();
    assert!(report.protected_execution_ids.contains(&execution));

    db.add_host("remote", "user@remote", 1, &[]).unwrap();
    insert_host_capability(&db, "remote", "driver=claude", "auto");
    let unrelated = create_test_chore(&db, product.id.clone(), "Unrelated work");
    let coordinator = coordinator_for(
        &db,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );

    // Unrelated work is admitted everywhere, the local host included.
    coordinator
        .validate_requested_host_for_test(&unrelated.id, "local")
        .unwrap();
    coordinator
        .validate_requested_host_for_test(&unrelated.id, "remote")
        .unwrap();
    assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());

    // The held work item is refused on every host, and the refusal says
    // which execution holds it and what lifts the hold.
    for host in ["local", "remote"] {
        let refusal = coordinator
            .validate_requested_host_for_test(&held_item, host)
            .unwrap_err()
            .to_string();
        assert!(refusal.contains(&execution), "{refusal}");
        assert!(refusal.contains("cancelled"), "{refusal}");
    }
    let refusal = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&held_item).build())
        .unwrap_err();
    assert!(refusal.to_string().contains("quarantined"), "{refusal:#}");
    assert_eq!(db.list_executions(Some(&held_item)).unwrap().len(), 1);
    assert!(!db.get_execution(&execution).unwrap().status.is_terminal());

    // And it really does dispatch: the unrelated chore starts a local run
    // while the hold is in place.
    dispatches_locally(&db, &coordinator, &unrelated.id).await;
    assert!(db.is_execution_quarantined(&execution).unwrap());
}

/// The incident: an execution is recorded running and no tmux identity ever
/// arrives, because the engine restarted while the spawn was still setting
/// up. It must fail alone, with a reason that says what happened, and every
/// other launch must go ahead.
#[tokio::test]
async fn a_spawn_whose_tmux_identity_never_arrives_fails_alone_and_other_launches_dispatch() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let (stranded, stranded_item) = seed_spawn_that_never_got_an_identity(&db, &product.id, "Launched mid-update");
    let unrelated = create_test_chore(&db, product.id.clone(), "Unrelated work");

    // The engine restarts. Startup judges the stranded run and applies its
    // verdict, exactly as `serve` does before any recovery.
    let report = quarantine_historical_local_workers(&db).unwrap();
    assert!(report.never_started_execution_ids.contains(&stranded));
    assert!(report.protected_execution_ids.is_empty());
    let failed = db
        .reap_startup_dead_execution_because(&stranded, &RunReconcileVerdict::Dead, SPAWN_NEVER_REACHED_TMUX_REASON)
        .unwrap()
        .expect("a never-started spawn is reaped at startup");

    // It ends terminal with a reason that names the cause.
    assert_eq!(failed.status, ExecutionStatus::Orphaned);
    let reason = failed.last_error.as_deref().unwrap_or_default();
    assert_eq!(reason, SPAWN_NEVER_REACHED_TMUX_REASON);
    assert!(reason.contains("never reached tmux"), "{reason}");
    assert!(reason.contains("no worker process ever started"), "{reason}");
    assert!(
        db.list_runs(&stranded)
            .unwrap()
            .iter()
            .all(|run| run.status == "orphaned")
    );

    // Nothing is held: not local dispatch, not the stranded work item.
    assert!(db.read_local_worker_quarantine().unwrap().is_empty());
    assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    let coordinator = coordinator_for(
        &db,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    coordinator
        .validate_requested_host_for_test(&unrelated.id, "local")
        .unwrap();
    coordinator
        .validate_requested_host_for_test(&stranded_item, "local")
        .unwrap();

    // Other launches dispatch, and the same work can be queued again.
    dispatches_locally(&db, &coordinator, &unrelated.id).await;
    let retry = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&stranded_item).build())
        .expect("the failed spawn must not keep its own work item from being queued again");
    assert_ne!(retry.id, stranded);
}

#[tokio::test]
async fn cancelling_a_held_execution_clears_its_gating_without_a_restart() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let (execution, held_item) = seed_unverifiable_worker(&db, &product.id, "Worker from before tmux hosting");
    quarantine_historical_local_workers(&db).unwrap();
    let coordinator = coordinator_for(
        &db,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    assert!(
        coordinator
            .validate_requested_host_for_test(&held_item, "local")
            .is_err()
    );
    assert!(
        db.list_attention_items_for_work_item(&held_item)
            .unwrap()
            .iter()
            .any(|item| item.kind == "local_worker_startup_quarantine" && item.status == "open")
    );

    db.cancel_execution(&execution).unwrap();

    // Same engine, same coordinator, no scan: the very next dispatch decision
    // admits the work item, drops the entry from the record and resolves the
    // attention item.
    coordinator
        .validate_requested_host_for_test(&held_item, "local")
        .unwrap();
    assert!(db.read_local_worker_quarantine().unwrap().is_empty());
    assert!(
        db.list_attention_items_for_work_item(&held_item)
            .unwrap()
            .iter()
            .all(|item| item.kind != "local_worker_startup_quarantine" || item.status == "resolved")
    );
    let retry = db
        .request_execution(RequestExecutionInput::builder().work_item_id(&held_item).build())
        .expect("a cancelled execution must not keep its work item from being queued again");
    assert_ne!(retry.id, execution);
}

/// The state the incident left behind, as an upgraded engine finds it: the
/// previous build recorded a hold naming a spawn that never reached tmux, and
/// the execution was cancelled afterwards.
#[tokio::test]
async fn a_hold_naming_a_cancelled_execution_gates_nothing_before_any_scan_runs() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let (stranded, stranded_item) = seed_spawn_that_never_got_an_identity(&db, &product.id, "Launched mid-update");
    let unrelated = create_test_chore(&db, product.id.clone(), "Unrelated work");
    db.set_metadata(
        "local_worker_startup_quarantine",
        &serde_json::json!({ "executions": { stranded.clone(): stranded_item.clone() }, "scan_error": null })
            .to_string(),
    )
    .unwrap();
    db.cancel_execution(&stranded).unwrap();

    let coordinator = coordinator_for(
        &db,
        Arc::new(FakeExecutionRunner {
            pending: true,
            ..FakeExecutionRunner::default()
        }),
    );
    // Nothing has re-judged the record yet, and nothing is gated by it.
    db.ensure_work_item_not_quarantined(&stranded_item).unwrap();
    assert!(db.local_dispatch_quarantine_reason().unwrap().is_none());
    coordinator
        .validate_requested_host_for_test(&stranded_item, "local")
        .unwrap();
    dispatches_locally(&db, &coordinator, &unrelated.id).await;
    // The first dispatch decision also tidied the stale record away.
    assert!(db.read_local_worker_quarantine().unwrap().is_empty());
}
