//! Convergence tests for a live worker whose execution the engine
//! terminalized — the 2026-07-28 "engine loses track of live workers" class.
//!
//! Each test drives a real `ServerState` against a real DB, because the whole
//! defect was that the engine's *derived* state disagreed with durable state:
//! a test that stubbed either half would assert the disagreement away.

use super::*;

use crate::test_support::*;
use crate::work::ExecutionStatus;

/// A pid guaranteed not to exist, so `kill(pid, 0)` returns `ESRCH`.
fn dead_pid() -> i64 {
    4_194_303
}

/// Seed a chore with a worker in the post-spawn shape, then terminalize its
/// execution the way a mis-fired reap does — leaving the process alive.
/// Returns `(work_item_id, execution_id)`.
fn stranded_live_worker(server_state: &ServerState, shell_pid: i64, reason: &str) -> (String, String) {
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_spawned_execution(db, &work_item_id, shell_pid);
    db.mark_execution_orphaned(&execution_id, reason).unwrap();
    (work_item_id, execution_id)
}

/// **Coverage: a live hook arriving for an execution already marked terminal.**
///
/// The engine has always *seen* this (it logs `SIG-4b`); what it did not do was
/// act. The hook is proof the engine's inference was wrong, so the execution
/// must come back out of its terminal status.
#[tokio::test]
async fn a_hook_for_an_orphaned_execution_readopts_it() {
    let (server_state, _dir) = test_server_state();
    // Our own pid stands in for the worker's still-running shell.
    let (work_item_id, execution_id) = stranded_live_worker(
        &server_state,
        i64::from(std::process::id()),
        "spawn-ack timeout; worker presumed dead",
    );
    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "precondition: the engine believes this run is dead",
    );

    // The worker, oblivious, emits a hook. No app session is registered, so
    // this also covers the degraded path where the live-state slot cannot be
    // restored — the row must still be re-adopted.
    let converged = crate::app::worker_events::converge_terminal_execution_contradiction(
        &server_state,
        &execution_id,
        "post_tool_use",
    )
    .await;
    assert!(
        converged,
        "a hook for a terminal execution must be handled, not dropped"
    );

    let after = server_state.work_db.get_execution(&execution_id).unwrap();
    assert_eq!(
        after.status,
        ExecutionStatus::WaitingHuman,
        "the run must return to the state a healthy pane-hosted worker occupies",
    );
    assert!(
        after.finished_at.is_none(),
        "a re-adopted run has not finished; a stale finished_at would keep it reading as over",
    );
    let item = server_state.work_db.get_work_item(&work_item_id).unwrap();
    let status = match &item {
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.status.clone(),
        other => panic!("expected a chore, got {other:?}"),
    };
    assert_eq!(
        status,
        boss_protocol::TaskStatus::Active,
        "the card must stay in Doing — a worker is genuinely working on it",
    );
}

/// The convergence must actually *end* the duplicate-dispatch storm, not just
/// rewrite a row. `waiting_human` is excluded from the orphan sweep's candidate
/// query, so re-adoption removes the item from re-dispatch consideration
/// entirely — asserted here by driving the real sweep afterwards.
#[tokio::test]
async fn readoption_stops_the_orphan_sweep_from_redispatching() {
    let (server_state, _dir) = test_server_state();
    let (work_item_id, execution_id) =
        stranded_live_worker(&server_state, i64::from(std::process::id()), "presumed dead");
    // Age the item past ORPHAN_MIN_AGE_SECS so it is a genuine sweep candidate.
    let old_epoch = boss_engine_utils::epoch_time::now_epoch_secs() - 600;
    server_state
        .work_db
        .force_updated_at_for_test(&work_item_id, old_epoch)
        .unwrap();

    crate::app::worker_events::converge_terminal_execution_contradiction(&server_state, &execution_id, "stop").await;

    let sink = crate::dispatch_events::RecordingDispatchEventSink::new();
    let outcome = crate::orphan_sweep::run_one_pass(
        server_state.work_db.as_ref(),
        server_state.execution_coordinator.clone(),
        &sink,
        &crate::worker_readoption::NoopLiveWorkerConvergence,
    )
    .await;

    assert_eq!(outcome.redispatched, 0, "a re-adopted row must not be re-dispatched");
    assert_eq!(
        outcome.live_process_skipped, 0,
        "the row should not even reach the durable-pid guard — re-adoption removed it from the \
         candidate set, which is the cheaper and more durable stop",
    );
    let executions = server_state.work_db.list_executions(Some(&work_item_id)).unwrap();
    assert_eq!(executions.len(), 1, "no second execution may be created");
}

/// A terminal status somebody actually *decided* is not reversible. A worker
/// that keeps hooking after `bossctl agents stop` cancelled its execution must
/// be reaped, not resurrected — otherwise the stop verb silently fails to stop
/// anything.
#[tokio::test]
async fn a_hook_for_a_cancelled_execution_reaps_instead_of_readopting() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    // A dead pid: the assertion here is about the VERDICT, and using a live pid
    // would have the reap signal this test process's own group.
    let execution_id = create_spawned_execution(db, &work_item_id, dead_pid());
    db.cancel_execution(&execution_id).unwrap();

    crate::app::worker_events::converge_terminal_execution_contradiction(&server_state, &execution_id, "stop").await;

    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Cancelled,
        "an operator's cancel must never be undone by the worker outliving it",
    );
}

/// The anti-duplication invariant at the convergence layer: once a replacement
/// worker is live on the row, the survivor is reaped even though its own
/// terminal status was only an inference. Re-adopting here is exactly how two
/// workers end up on one chore.
#[tokio::test]
async fn a_survivor_is_reaped_when_a_replacement_execution_is_already_live() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let stranded = create_spawned_execution(db, &work_item_id, dead_pid());
    db.mark_execution_orphaned(&stranded, "presumed dead").unwrap();
    // A replacement was dispatched and is now live.
    let replacement = create_spawned_execution(db, &work_item_id, dead_pid());

    crate::app::worker_events::converge_terminal_execution_contradiction(&server_state, &stranded, "stop").await;

    assert_eq!(
        server_state.work_db.get_execution(&stranded).unwrap().status,
        ExecutionStatus::Orphaned,
        "the survivor's row stays terminal — the live replacement owns the work item now",
    );
    assert_eq!(
        server_state.work_db.get_execution(&replacement).unwrap().status,
        ExecutionStatus::WaitingHuman,
        "the live replacement must be left completely alone",
    );
}

/// **Coverage: a worker whose spawn ack is lost but whose process is alive.**
///
/// The full production sequence, driven end to end:
///
/// 1. The `SpawnWorkerPane` ack is lost, so the slot is registered
///    provisionally with `shell_pid = 0` (`spawn_flow`'s ack-timeout branch).
/// 2. The app hosted the pane anyway and the worker starts; the real pid lands
///    via `UpdateWorkerShellPid`, which persists it to `work_runs`.
/// 3. Nothing else reports in before the grace window expires and a reap
///    orphans the execution — the engine now believes a running worker is dead.
/// 4. The worker's next hook re-adopts it.
///
/// Step 4 is the convergence the degraded network must not be able to defeat:
/// a lost ack may cost tracking temporarily, but it must not produce a
/// permanently untracked worker.
#[tokio::test]
async fn a_worker_whose_spawn_ack_was_lost_is_readopted_once_it_hooks() {
    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    let execution_id = create_old_execution(db, &work_item_id);

    // (1) Provisional registration: the ack never arrived, so no pid is known.
    server_state.live_worker_states.register_spawn(
        1,
        execution_id.clone(),
        "claude-opus-4-7",
        0,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "test chore".to_owned(),
            execution_id: execution_id.clone(),
        }),
    );
    let (_exec, run) = db
        .start_execution_run(&execution_id, "worker-1", "repo-1", "lease-1", "ws-1", "/tmp/ws")
        .unwrap();

    // (2) The pane came up regardless and the app reported the real pid.
    let live_pid = i64::from(std::process::id());
    assert!(
        db.set_run_shell_pid_for_execution(&execution_id, live_pid).unwrap(),
        "the durable pid write is what makes this recoverable at all",
    );
    finish_run_waiting_human(db, &execution_id, &run.id, Some("Spawned worker pane in slot 1."));

    // (3) A reap concludes the worker never came up. This is the false
    //     inference the degraded network produces.
    db.mark_execution_orphaned(
        &execution_id,
        "spawn-ack-timeout: no shell pid reported and no hook event received within 60s of spawn",
    )
    .unwrap();
    server_state.live_worker_states.release_slot(1);
    assert!(
        server_state.worker_registry.slot_for_run(&execution_id).is_none(),
        "precondition: the engine has no slot mapping left for this run",
    );

    // (4) The worker — which never stopped running — emits its next hook.
    crate::app::worker_events::converge_terminal_execution_contradiction(&server_state, &execution_id, "session_start")
        .await;

    let after = db.get_execution(&execution_id).unwrap();
    assert_eq!(
        after.status,
        ExecutionStatus::WaitingHuman,
        "a lost ack must not be able to leave a running worker permanently untracked",
    );
    assert!(after.finished_at.is_none());
}

/// Re-adoption must derive `awaiting_input_capable` from the run's own driver,
/// the way `spawn_flow` does at spawn time. Hardcoding Claude's `true` here
/// would let `mark_stalled_spawns` promote a re-adopted Codex worker to
/// `WaitingForInput` — a driver that cannot signal awaiting-input at all —
/// which is the wrong-indicator class this whole path exists to end.
#[tokio::test]
async fn readoption_derives_the_awaiting_input_capability_from_the_runs_driver() {
    use crate::work::WorkItemPatch;

    let (server_state, _dir) = test_server_state();
    let db = server_state.work_db.as_ref();
    let product_id = create_product(db);
    let work_item_id = create_active_chore(db, &product_id, "test chore");
    // A driver that does NOT provide AwaitingInputSignal. Claude does, so a
    // hardcoded capability would pass against the default driver either way.
    db.update_work_item(
        &work_item_id,
        WorkItemPatch {
            driver: Some("codex".to_owned()),
            ..Default::default()
        },
    )
    .unwrap();
    let execution_id = create_spawned_execution(db, &work_item_id, i64::from(std::process::id()));
    db.mark_execution_orphaned(&execution_id, "presumed dead").unwrap();
    let execution = db.get_execution(&execution_id).unwrap();

    // The live-state slot is only restored when the app can say which slot
    // hosts the pane, so stand one up and answer the probe.
    let sink = make_session_sink();
    server_state
        .register_app_session("session-app".into(), sink.clone())
        .await;
    let server_clone = server_state.clone();
    let execution_clone = execution.clone();
    let converge = tokio::spawn(async move {
        server_clone
            .converge_terminal_execution(&execution_clone, "hook_after_terminal")
            .await
    });

    let envelope = sink.next().await.expect("an EngineRequest event should be enqueued");
    let request_id = match envelope.payload {
        FrontendEvent::EngineRequest { request_id, .. } => request_id,
        other => panic!("expected EngineRequest, got {other:?}"),
    };
    server_state
        .deliver_app_response(
            "session-app",
            &request_id,
            EngineToAppResponse::ListHostedPanes {
                result: Ok(crate::protocol::ListHostedPanesResult {
                    panes: vec![crate::protocol::HostedPaneEntry {
                        slot_id: 4,
                        run_id: execution_id.clone(),
                        summary: None,
                        task_title: None,
                    }],
                }),
            },
        )
        .await;
    assert_eq!(converge.await.expect("converge task"), "readopt");

    assert!(
        !server_state.live_worker_states.awaiting_input_capable(4),
        "a re-adopted Codex worker must not be paintable as awaiting input",
    );
    let state = server_state
        .live_worker_states
        .get(4)
        .expect("the re-adopted slot must carry a live-state entry");
    assert_eq!(
        state.model, "OpenAI Codex",
        "the label must name the run's resolved driver, not a hardcoded `claude`",
    );

    // Re-adoption re-registers an already-running worker, so the `spawned_at`
    // it stamps is the moment the engine noticed, not the moment anything
    // exec'd. Driver-start verification must not age that fiction and reap a
    // live worker; and the hook that triggered this convergence is genuine
    // driver-originated proof, so it is recorded rather than discarded.
    assert_eq!(
        server_state.live_worker_states.driver_start_expectation(4),
        Some(crate::live_worker_state::DriverStartExpectation::Readopted),
    );
    assert!(
        server_state.live_worker_states.driver_signal_at(4).is_some(),
        "the hook that triggered the re-adoption is driver-start proof",
    );
    assert!(
        server_state
            .live_worker_states
            .unverified_driver_starts(
                boss_engine_utils::epoch_time::now_epoch_secs()
                    + crate::live_worker_state::DRIVER_START_GRACE_SECS
                    + 60,
                crate::live_worker_state::DRIVER_START_GRACE_SECS,
            )
            .is_empty(),
        "a re-adopted worker must never be reaped as a driver that never started",
    );
}

/// Convergence is serialized per run. A worker mid-turn emits several hooks a
/// second, and each one reaching this path independently would race writes
/// against the same execution row and fan out `ListHostedPanes` round-trips at
/// the app.
#[tokio::test]
async fn concurrent_hooks_for_one_run_converge_once() {
    let (server_state, _dir) = test_server_state();
    let (_work_item_id, execution_id) =
        stranded_live_worker(&server_state, i64::from(std::process::id()), "presumed dead");
    let execution = server_state.work_db.get_execution(&execution_id).unwrap();

    // Hold the latch exactly as an in-flight resolution on another task would.
    // Taking it directly rather than racing two futures is what makes the
    // assertion mean something: an `await`-ordering race can let the second
    // call arrive after the first has already finished, in which case it
    // returns `no_contradiction` (the row is no longer terminal) and the test
    // passes with the latch deleted.
    assert!(
        server_state.begin_terminal_convergence(&execution_id),
        "precondition: the latch is free before anyone claims it",
    );

    assert_eq!(
        server_state
            .converge_terminal_execution(&execution, "hook_after_terminal")
            .await,
        "in_flight",
        "a second resolution while one is in flight must be refused, not run",
    );
    assert!(
        server_state
            .work_db
            .get_execution(&execution_id)
            .unwrap()
            .status
            .is_terminal(),
        "the refused call must not have touched the execution row",
    );

    // The latch is released when the in-flight resolution ends, and the run is
    // then convergeable again — a permanently-stuck latch would strand the row
    // just as surely as the duplicate writes it prevents.
    server_state.end_terminal_convergence(&execution_id);
    assert_eq!(
        server_state
            .converge_terminal_execution(&execution, "hook_after_terminal")
            .await,
        "readopt",
        "once the latch is free the contradiction must actually be resolved",
    );
    assert_eq!(
        server_state.work_db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::WaitingHuman,
    );
}
