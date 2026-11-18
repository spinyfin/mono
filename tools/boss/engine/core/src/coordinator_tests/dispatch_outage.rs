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
    // that item, while the host itself is still accepting launches. This is
    // the documented narrowing of the outage definition: placement is not
    // checked across every queued item, only host-level exclusions are.
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

#[tokio::test]
async fn a_failed_startup_scan_holds_every_item_and_is_reported_as_an_outage() {
    let fixture = fixture(2);
    fixture
        .db
        .write_local_worker_quarantine(&LocalWorkerQuarantine {
            executions: Default::default(),
            scan_error: Some("tmux list-panes failed".to_owned()),
        })
        .unwrap();
    fixture.db.set_host_enabled("anaplian", false).unwrap();

    for execution in &fixture.executions {
        dispatch(&fixture, execution).await.unwrap_err();
    }

    let outage = fixture.coordinator.dispatch_outage().active().expect("outage raised");
    assert_eq!(outage.refused_attempts, 2);
    let local = outage.host_exclusions.iter().find(|h| h.host_id == "local").unwrap();
    assert!(local.cause.contains("tmux list-panes failed"), "{}", local.cause);
}

#[tokio::test]
async fn reconciling_a_held_spoke_execution_on_a_disabled_remote_does_not_end_the_outage() {
    let fixture = fixture(2);
    hold_local_dispatch(&fixture.db);
    fixture.db.set_host_enabled("anaplian", false).unwrap();
    dispatch(&fixture, &fixture.executions[0]).await.unwrap_err();
    let before = fixture.coordinator.dispatch_outage().active().expect("outage raised");

    // The second execution already owns an assignment on the disabled remote,
    // so reconciling it returns that host without selecting one.
    fixture
        .db
        .reserve_spoke_assignment(&fixture.executions[1].id, "anaplian")
        .unwrap();
    let _ = dispatch(&fixture, &fixture.executions[1]).await;

    let after = fixture
        .coordinator
        .dispatch_outage()
        .active()
        .expect("outage stays active");
    assert_eq!(after.started_epoch_s, before.started_epoch_s);
    assert!(
        !engine_stages(&fixture)
            .await
            .contains(&"dispatch_outage_recovered".to_owned()),
        "a held-spoke reconciliation is not a new claim",
    );
}

fn hold_every_item_with_a_scan_error(db: &WorkDb) {
    db.write_local_worker_quarantine(&LocalWorkerQuarantine {
        executions: Default::default(),
        scan_error: Some("tmux list-panes failed".to_owned()),
    })
    .unwrap();
}

#[tokio::test]
async fn a_launch_refused_at_admission_by_a_startup_scan_failure_raises_the_outage() {
    // The hold is in place before anything is requested, so the refusal
    // happens at admission: no execution row is created, and host selection
    // is never reached.
    let fixture = fixture(1);
    hold_every_item_with_a_scan_error(&fixture.db);
    assert_eq!(fixture.coordinator.dispatch_outage().active(), None);

    let err = fixture
        .coordinator
        .request_execution_via_db(
            RequestExecutionInput::builder()
                .work_item_id(fixture.executions[0].work_item_id.clone())
                .build(),
            Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new()),
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("quarantined"), "{err:#}");

    let outage = fixture
        .coordinator
        .dispatch_outage()
        .active()
        .expect("the admission refusal raises the outage");
    assert_eq!(outage.refused_attempts, 1);
    assert_eq!(outage.refused_work_items, 1);
    assert_eq!(outage.latest_work_item_id, fixture.executions[0].work_item_id);
    let local = outage.host_exclusions.iter().find(|h| h.host_id == "local").unwrap();
    assert!(
        local.cause.contains("tmux list-panes failed"),
        "the scan error is carried verbatim: {}",
        local.cause
    );

    // The events go out on a spawned task; both are replayable.
    let mut stages = Vec::new();
    for _ in 0..200 {
        stages = engine_stages(&fixture).await;
        if stages.len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(stages, ["host_selected", "dispatch_outage_started"]);
    let replay = boss_execution_contracts::replay_outage_events(&fixture.events.events().await);
    assert_eq!(replay.active.expect("the stream shows the outage").refused_attempts, 1);
}

#[tokio::test]
async fn the_startup_sweep_reports_active_work_a_scan_failure_is_holding_back() {
    let fixture = fixture(1);
    // Healthy: nothing held, nothing reported.
    fixture.coordinator.note_startup_scan_hold();
    assert_eq!(fixture.coordinator.dispatch_outage().active(), None);

    // The chore is active work with no live worker, which reconcile skips
    // under the hold.
    fixture
        .db
        .force_task_status_for_test(&fixture.executions[0].work_item_id, "active")
        .unwrap();
    hold_every_item_with_a_scan_error(&fixture.db);
    fixture.coordinator.note_startup_scan_hold();

    let outage = fixture.coordinator.dispatch_outage().active().expect("outage raised");
    assert_eq!(outage.refused_work_items, 1);
}
