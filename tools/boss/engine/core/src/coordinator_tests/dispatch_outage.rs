//! Global dispatch outage: every host excluded for every item. Detection is a
//! property of the whole registry, recovery is a successful claim, and the
//! per-item failure handling is untouched by either.
use super::helpers::*;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::work::LocalWorkerQuarantine;

/// A hold on the local host naming an execution that is not any of the items
/// the tests dispatch, so only the host-level exclusion applies to them.
fn hold_local_dispatch(db: &WorkDb) {
    db.write_local_worker_quarantine(&LocalWorkerQuarantine {
        executions: [("exec_held".to_owned(), "task_held".to_owned())].into(),
        scan_error: None,
    })
    .unwrap();
}

struct Fixture {
    db: Arc<WorkDb>,
    coordinator: Arc<ExecutionCoordinator>,
    events: Arc<RecordingDispatchEventSink>,
    executions: Vec<WorkExecution>,
    _dir: tempfile::TempDir,
}

/// `chores` queued chores on a registry of the local host and one remote.
fn fixture(chores: usize) -> Fixture {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    db.add_host("anaplian", "user@anaplian", 2, &[]).unwrap();
    seed_local_claude_driver(&db);
    let product = create_test_product(&db);
    let executions = (0..chores)
        .map(|n| {
            let chore = create_test_chore(&db, product.id.clone(), format!("Chore {n}"));
            db.reconcile_product_executions(&product.id).unwrap();
            db.list_executions(Some(&chore.id)).unwrap().pop().unwrap()
        })
        .collect();
    let events = Arc::new(RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(4),
            Arc::new(FakeCubeClient::default()),
            Arc::new(FakeExecutionRunner {
                pending: true,
                ..FakeExecutionRunner::default()
            }),
        )
        .with_dispatch_events(events.clone())
        .with_pre_start_retry_delays(Vec::new()),
    );
    Fixture {
        db,
        coordinator,
        events,
        executions,
        _dir: dir,
    }
}

async fn dispatch(fixture: &Fixture, execution: &WorkExecution) -> Result<()> {
    let worker_id = fixture
        .coordinator
        .worker_pool()
        .claim_worker(&execution.id, None)
        .await
        .expect("worker available");
    fixture
        .coordinator
        .schedule_execution_for_test(execution, &worker_id, DispatchAdmission::Queued)
        .await
}

async fn engine_stages(fixture: &Fixture) -> Vec<String> {
    fixture
        .events
        .events_for("engine")
        .await
        .into_iter()
        .map(|event| event.stage)
        .collect()
}

#[tokio::test]
async fn every_host_excluded_raises_one_outage_with_each_hosts_reason_and_recovery() {
    let fixture = fixture(1);
    hold_local_dispatch(&fixture.db);
    fixture.db.set_host_enabled("anaplian", false).unwrap();

    dispatch(&fixture, &fixture.executions[0]).await.unwrap_err();

    let outage = fixture.coordinator.dispatch_outage().active().expect("outage raised");
    assert_eq!(outage.refused_attempts, 1);
    assert_eq!(outage.refused_work_items, 1);
    let local = outage.host_exclusions.iter().find(|h| h.host_id == "local").unwrap();
    assert_eq!(local.kind, "local_quarantine");
    assert!(
        local.reason.contains("local dispatch quarantined"),
        "verbatim: {}",
        local.reason
    );
    assert!(local.cause.contains("exec_held"), "{}", local.cause);
    assert!(
        local
            .recovery_action
            .as_deref()
            .unwrap()
            .contains("local_worker_startup_quarantine")
    );
    let remote = outage.host_exclusions.iter().find(|h| h.host_id == "anaplian").unwrap();
    assert_eq!(remote.kind, "host_disabled");
    assert!(
        remote
            .recovery_action
            .as_deref()
            .unwrap()
            .contains("bossctl hosts enable anaplian")
    );
    assert_eq!(engine_stages(&fixture).await, ["dispatch_outage_started"]);
}

#[tokio::test]
async fn an_item_that_fits_no_host_while_another_host_is_usable_is_not_an_outage() {
    let fixture = fixture(1);
    // The remote is disabled but local is healthy: this item is refused only
    // because it is pinned to the disabled remote.
    fixture.db.set_host_enabled("anaplian", false).unwrap();
    fixture
        .db
        .set_execution_pinned_host(&fixture.executions[0].id, Some("anaplian"))
        .unwrap();

    dispatch(&fixture, &fixture.executions[0]).await.unwrap_err();

    assert_eq!(fixture.coordinator.dispatch_outage().active(), None);
    assert!(engine_stages(&fixture).await.is_empty());
}

#[tokio::test]
async fn a_held_local_host_with_a_usable_remote_is_not_an_outage() {
    let fixture = fixture(1);
    hold_local_dispatch(&fixture.db);
    // Remote stays enabled but lacks the driver the item needs: refused for
    // that item, while the host itself is still accepting launches.
    fixture
        .db
        .replace_auto_host_capabilities("anaplian", &["drivers-probed=true".to_owned()])
        .unwrap();

    dispatch(&fixture, &fixture.executions[0]).await.unwrap_err();

    assert_eq!(fixture.coordinator.dispatch_outage().active(), None);
}

#[tokio::test]
async fn repeated_refusals_fold_into_one_record_and_leave_each_items_own_failure_intact() {
    let fixture = fixture(3);
    hold_local_dispatch(&fixture.db);
    fixture.db.set_host_enabled("anaplian", false).unwrap();

    for execution in &fixture.executions {
        dispatch(&fixture, execution).await.unwrap_err();
    }

    // (Repeated refusals of one item fold the same way; that is pinned in the
    // tracker's own tests, where an item can be refused again without its
    // execution having gone terminal.)
    let outage = fixture.coordinator.dispatch_outage().active().unwrap();
    assert_eq!(outage.refused_attempts, 3);
    assert_eq!(outage.refused_work_items, 3);
    assert_eq!(
        engine_stages(&fixture).await,
        ["dispatch_outage_started"],
        "one outage event however many attempts it absorbs",
    );
    // Aggregating must not suppress the per-item errors.
    for execution in &fixture.executions {
        let events = fixture.events.events_for(&execution.id).await;
        assert!(
            events
                .iter()
                .any(|e| e.stage == "host_selected" && e.outcome == "error"),
            "{} keeps its own host_selected:error: {events:#?}",
            execution.id,
        );
        let items = fixture.db.list_attention_items(&execution.id).unwrap();
        assert!(items.iter().any(|i| i.kind == "no_eligible_host"), "{items:?}");
    }
}

#[tokio::test]
async fn the_first_successful_claim_ends_the_outage_and_records_how_long_it_lasted() {
    let fixture = fixture(2);
    hold_local_dispatch(&fixture.db);
    fixture.db.set_host_enabled("anaplian", false).unwrap();
    dispatch(&fixture, &fixture.executions[0]).await.unwrap_err();
    assert!(fixture.coordinator.dispatch_outage().active().is_some());

    // Clearing the hold changes a flag, not the outage: nothing has launched.
    fixture
        .db
        .write_local_worker_quarantine(&LocalWorkerQuarantine::default())
        .unwrap();
    assert!(
        fixture.coordinator.dispatch_outage().active().is_some(),
        "a cleared flag is not recovery",
    );

    // A new dispatch claims the local host: that is the recovery.
    let _ = dispatch(&fixture, &fixture.executions[1]).await;
    assert_eq!(fixture.coordinator.dispatch_outage().active(), None);

    let events = fixture.events.events_for("engine").await;
    let recovered = events
        .iter()
        .find(|e| e.stage == "dispatch_outage_recovered")
        .unwrap_or_else(|| panic!("expected a recovery event: {events:#?}"));
    assert_eq!(recovered.outcome, "ok");
    assert!(recovered.details["duration_secs"].as_i64().unwrap() >= 0);
    assert_eq!(recovered.details["refused_attempts"], 1);
    assert_eq!(
        recovered.details["claimed_by_execution_id"],
        fixture.executions[1].id.as_str()
    );
    assert_eq!(recovered.details["claimed_host_id"], "local");
}
