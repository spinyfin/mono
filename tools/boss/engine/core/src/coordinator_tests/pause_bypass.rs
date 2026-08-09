//! The pause-only forced-dispatch override — `RequestExecutionInput::
//! bypass_dispatch_pause`, `ExecutionCoordinator::evaluate_dispatch_admission`,
//! and `ExecutionCoordinator::dispatch_with_pause_bypass`. See
//! `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
//!
//! Shared fixtures live in [`super::helpers`].

use super::helpers::*;

use boss_protocol::{AddDependencyInput, DispatchAdmissionEntryPoint, PauseReason};

fn live_states() -> Arc<crate::live_worker_state::LiveWorkerStateRegistry> {
    Arc::new(crate::live_worker_state::LiveWorkerStateRegistry::new())
}

fn force_input(work_item_id: impl Into<String>, observed_pause_since_epoch_s: Option<u64>) -> RequestExecutionInput {
    RequestExecutionInput::builder()
        .work_item_id(work_item_id)
        .bypass_dispatch_pause(true)
        .entry_point(DispatchAdmissionEntryPoint::Cli)
        .maybe_observed_pause_since_epoch_s(observed_pause_since_epoch_s)
        .build()
}

/// A held main-pool row must dispatch when `bypass_dispatch_pause` is set
/// against a fresh operator pause, and the admitted-through-a-pause
/// dispatch event must be recorded with the pause's own reason/origin.
#[tokio::test]
async fn force_start_dispatches_past_operator_pause_and_records_override_event() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Held by pause");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();
    assert_eq!(execution.status, ExecutionStatus::Ready);

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone()),
    );

    coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: investigating worker failures").unwrap(),
    );

    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&chore.id, None), live_states())
        .await;
    let _dispatched = result.expect("force-start must succeed past an operator pause");
    // The row is claimed synchronously; the spawn tail (and the status
    // flip to `running`) happens in a handed-off task — see
    // `dispatch_with_pause_bypass`'s doc on why this doesn't wait for it.
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;
    assert_eq!(runner.calls.lock().await.len(), 1, "the worker must actually spawn");

    // The pause itself is untouched — force overrides admission for this
    // one row only, never the global switch.
    assert!(
        coordinator.is_dispatch_paused(),
        "a successful override must not resume global dispatch"
    );

    let events = recording.events_for(&execution.id).await;
    let override_event = events
        .iter()
        .find(|e| e.stage == crate::dispatch_events::Stage::DispatchPauseOverride.as_str())
        .unwrap_or_else(|| panic!("expected a dispatch_pause_override event, got: {events:?}"));
    assert_eq!(override_event.outcome, crate::dispatch_events::Outcome::Ok.as_str());
    let details = &override_event.details;
    assert_eq!(details["entry_point"], "cli");
    assert_eq!(details["pause_origin"], "operator");
}

/// A breaker-originated pause is never overridable: force-start must
/// refuse outright, and — because this row was freshly created by the
/// forced request itself — the row must not linger `ready` for a later
/// drain to pick up once the breaker pause clears. No residue.
#[tokio::test]
async fn force_start_refused_by_breaker_pause_leaves_no_residue() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    // `autostart(false)` (create_test_chore_manual) so no ready execution
    // exists yet — the forced request itself must be what creates it.
    let chore = create_test_chore_manual(&db, product.id.clone(), "No prior execution");
    assert!(db.list_executions(Some(&chore.id)).unwrap().is_empty());

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone()),
    );

    coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Breaker,
        PauseReason::new("test: spawn path unhealthy").unwrap(),
    );

    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&chore.id, None), live_states())
        .await;
    assert!(result.is_err(), "a breaker pause must never be overridable");
    assert!(
        result.unwrap_err().to_string().contains("not overridable"),
        "refusal must name the pause as non-overridable"
    );
    assert_eq!(runner.calls.lock().await.len(), 0, "no worker may spawn");

    // No residue: the row this request would have created must not be
    // left `ready` (which would dispatch later by surprise once the
    // breaker pause clears).
    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert!(
        executions.iter().all(|e| e.status != ExecutionStatus::Ready),
        "a refused forced request must leave no ready residue, got: {executions:?}"
    );
}

/// The interactive concurrency cap is not overridable either: a
/// force-start against a fresh, otherwise-overridable operator pause
/// must still refuse once the cap blocks it, and must leave no residue
/// for the freshly-created row.
#[tokio::test]
async fn force_start_refused_by_interactive_cap_leaves_no_residue() {
    use crate::coordinator::MAX_CONCURRENT_INTERACTIVE_WORKERS;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore_manual(&db, product.id.clone(), "Capped");

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(
            db.clone(),
            WorkerPool::new(MAX_CONCURRENT_INTERACTIVE_WORKERS + 4),
            cube.clone(),
            runner.clone(),
        )
        .with_pre_start_retry_delays(Vec::new()),
    );
    for i in 0..MAX_CONCURRENT_INTERACTIVE_WORKERS {
        coordinator
            .worker_pool()
            .claim_worker(&format!("exec-busy-{i}"), None)
            .await
            .expect("idle slot below the cap");
    }

    coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause").unwrap(),
    );

    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&chore.id, None), live_states())
        .await;
    assert!(result.is_err(), "the interactive cap must refuse even a forced request");
    assert!(
        result.unwrap_err().to_string().contains("interactive concurrency cap"),
        "refusal must name the interactive concurrency cap"
    );
    assert_eq!(runner.calls.lock().await.len(), 0);

    let executions = db.list_executions(Some(&chore.id)).unwrap();
    assert!(
        executions.iter().all(|e| e.status != ExecutionStatus::Ready),
        "a cap-refused forced request must leave no ready residue, got: {executions:?}"
    );
}

/// A work item gated by an unmet dependency must be refused even with
/// force — dependency gating is unconditionally enforced upstream of the
/// pause-bypass path (`request_execution_in_tx_with_live_check`), so no
/// row is ever created in the first place.
#[tokio::test]
async fn force_start_refused_by_unmet_dependency_no_residue() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let prereq = create_test_chore_manual(&db, product.id.clone(), "Prereq");
    let dependent = create_test_chore_manual(&db, product.id.clone(), "Dependent");
    db.add_dependency(AddDependencyInput {
        dependent: dependent.id.clone(),
        prerequisite: prereq.id.clone(),
        relation: None,
    })
    .unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    // No pause active at all — this exercises the "blocked regardless of
    // the pause" path: force must not paper over an unmet dependency
    // whether or not there is anything to override.
    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&dependent.id, None), live_states())
        .await;
    assert!(result.is_err(), "an unmet dependency must refuse even with force");
    assert!(
        result.unwrap_err().to_string().to_lowercase().contains("depend"),
        "refusal must name the dependency blocker"
    );
    assert!(db.list_executions(Some(&dependent.id)).unwrap().is_empty());
}

/// If the pause's generation changed since the caller last observed it
/// (a different `paused_since_epoch_s`, e.g. resumed and re-paused with a
/// new reason), a confirmation carrying the stale generation must be
/// refused rather than silently acted on — the pause-generation race the
/// design calls out.
#[tokio::test]
async fn force_start_stale_pause_generation_is_refused() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Held by pause");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: current pause").unwrap(),
    );

    // The caller observed a DIFFERENT (older) generation than what is
    // active now.
    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&chore.id, Some(1_600_000_000)), live_states())
        .await;
    assert!(result.is_err(), "a stale confirmed pause generation must be refused");
    assert!(
        result.unwrap_err().to_string().contains("changed"),
        "refusal must explain the pause changed since it was observed"
    );
    assert_eq!(runner.calls.lock().await.len(), 0);
    assert_eq!(
        db.get_execution(&db.list_executions(Some(&chore.id)).unwrap()[0].id)
            .unwrap()
            .status,
        ExecutionStatus::Ready,
        "a pre-existing row must be left unchanged by a stale-generation refusal"
    );
}

/// If the pause was lifted entirely between the app's evaluation and its
/// confirmed drop, the request proceeds as an ordinary (non-forced)
/// start — no override event, no synchronous wait, exactly the design's
/// "pause was lifted" handling.
#[tokio::test]
async fn force_start_proceeds_ordinarily_when_pause_already_lifted() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Never paused");
    db.reconcile_product_executions(&product.id).unwrap();
    let execution = db.list_executions(Some(&chore.id)).unwrap().pop().unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let recording = Arc::new(crate::dispatch_events::RecordingDispatchEventSink::new());
    let coordinator = Arc::new(
        ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone())
            .with_dispatch_events(recording.clone()),
    );

    // No pause active — as if the app observed one earlier and it was
    // since resumed.
    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&chore.id, Some(1_700_000_000)), live_states())
        .await;
    let dispatched = result.expect("must proceed as an ordinary start once the pause is gone");
    // The ordinary path is fire-and-forget (`kick()`, not awaited) — the
    // returned row may still read `ready` the instant this call returns;
    // it must still reach `running` shortly via the normal scheduler.
    assert_eq!(dispatched.status, ExecutionStatus::Ready);
    wait_for_execution_status(db.as_ref(), &execution.id, ExecutionStatus::Running).await;

    let events = recording.events_for(&execution.id).await;
    assert!(
        events.iter().all(
            |e| e.stage != crate::dispatch_events::Stage::DispatchPauseOverride.as_str()
                && e.stage != crate::dispatch_events::Stage::DispatchPauseOverrideRefused.as_str()
        ),
        "no override/refusal event should be recorded when there was nothing to override, got: {events:?}"
    );
}

/// Force must route through the same pool-correct claim path as ordinary
/// dispatch: an automation-sourced work item forced past an operator
/// pause must still claim an automation-pool (`auto-worker-`) slot, not a
/// main-pool one — the exact regression `force_dispatch`'s pool-
/// classification bug produced for PR reviews (mono#2686 background).
#[tokio::test]
async fn force_start_preserves_automation_pool_routing() {
    use crate::work::CreateAutomationInput;

    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let automation = db
        .create_automation(CreateAutomationInput {
            product_id: product.id.clone(),
            name: "Test automation".to_owned(),
            repo_remote_url: None,
            trigger: boss_protocol::AutomationTrigger::Schedule {
                cron: "0 14 * * 1-5".to_owned(),
                timezone: "UTC".to_owned(),
            },
            standing_instruction: "do maintenance".to_owned(),
            open_task_limit: 1,
            catch_up_window_secs: None,
            enabled: true,
            created_via: None,
        })
        .unwrap();
    let auto_chore = create_test_chore(&db, product.id.clone(), "Automation chore");
    {
        let conn = db.connect().unwrap();
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?1 WHERE id = ?2",
            rusqlite::params![automation.id, auto_chore.id],
        )
        .unwrap();
    }
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let mut coord = ExecutionCoordinator::new(db.clone(), WorkerPool::new(1), cube.clone(), runner.clone());
    coord.set_automation_pool(WorkerPool::new_automation(1));
    let coordinator = Arc::new(coord);

    coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause").unwrap(),
    );

    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&auto_chore.id, None), live_states())
        .await;
    let dispatched = result.expect("force-start must dispatch the automation-sourced item");
    wait_for_execution_status(db.as_ref(), &dispatched.id, ExecutionStatus::Running).await;

    let calls = runner.calls.lock().await;
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].0.starts_with(AUTOMATION_WORKER_ID_PREFIX),
        "an automation-sourced item must claim an automation-pool slot even under force, got worker id {:?}",
        calls[0].0
    );
    // The main pool's one slot must be untouched.
    assert_eq!(coordinator.worker_pool().idle_count().await, 1);
}

/// `autostart: false` and a churn-guard-parked attention item are
/// reported by the evaluator as informational blockers, but neither one
/// is a NEW reason force refuses — an explicit start (forced or not) has
/// always bypassed both. Confirms the design's non-goal: "a forced
/// explicit start retains those same behaviors, but the new force bit is
/// not what authorizes them."
#[tokio::test]
async fn force_start_bypasses_autostart_and_churn_guard_informationally() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    // autostart = false, still `todo` — never auto-dispatched.
    let chore = create_test_chore_manual(&db, product.id.clone(), "Autostart disabled");
    assert!(db.list_executions(Some(&chore.id)).unwrap().is_empty());

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    // Sanity: the evaluator reports the informational blocker...
    let admission = coordinator.evaluate_dispatch_admission(&chore.id).await.unwrap();
    assert!(
        admission
            .blockers
            .iter()
            .any(|b| b.code == boss_protocol::ADMISSION_BLOCKER_AUTOSTART_DISABLED),
        "autostart_disabled must be reported for transparency: {:?}",
        admission.blockers
    );

    coordinator.pause_dispatch(
        1_700_000_000,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: operator pause").unwrap(),
    );

    // ...but force-start still succeeds despite it.
    let result = coordinator
        .dispatch_with_pause_bypass(force_input(&chore.id, None), live_states())
        .await;
    let dispatched = result.expect("autostart:false must never refuse an explicit forced start");
    wait_for_execution_status(db.as_ref(), &dispatched.id, ExecutionStatus::Running).await;
}

/// `EvaluateDispatchAdmission` reports an active, overridable pause with
/// its reason/origin/generation, and reports `would_dispatch = true` once
/// the pause is lifted and nothing else blocks.
#[tokio::test]
async fn evaluate_dispatch_admission_reports_pause_snapshot() {
    let dir = tempdir().unwrap();
    let db = Arc::new(WorkDb::open(dir.path().join("boss.db")).unwrap());
    let product = create_test_product(&db);
    let chore = create_test_chore(&db, product.id.clone(), "Plain chore");
    db.reconcile_product_executions(&product.id).unwrap();

    let cube = Arc::new(FakeCubeClient::default());
    let runner = Arc::new(FakeExecutionRunner::default());
    let coordinator = Arc::new(ExecutionCoordinator::new(
        db.clone(),
        WorkerPool::new(1),
        cube.clone(),
        runner.clone(),
    ));

    let admission = coordinator.evaluate_dispatch_admission(&chore.id).await.unwrap();
    assert!(admission.would_dispatch, "nothing should block an ordinary ready chore");
    assert!(!admission.pause.active);

    coordinator.pause_dispatch(
        1_700_000_042,
        DispatchPauseOrigin::Operator,
        PauseReason::new("test: investigating worker failures").unwrap(),
    );
    let paused_admission = coordinator.evaluate_dispatch_admission(&chore.id).await.unwrap();
    assert!(!paused_admission.would_dispatch);
    assert!(paused_admission.pause.active);
    assert!(paused_admission.pause.overridable);
    assert_eq!(paused_admission.pause.paused_since_epoch_s, Some(1_700_000_042));
    assert_eq!(
        paused_admission.pause.reason.as_deref(),
        Some("test: investigating worker failures")
    );
    assert!(
        paused_admission.blockers.is_empty(),
        "no other blocker should be present"
    );
}
