//! Completion-teardown vs. reconciler-sweep race.
//!
//! These tests pin the invariant the 2026-07-30 incident violated: **a
//! pane must not be reclaimable by a sweep while its own completion
//! teardown is still in flight.** The completion path terminalizes the
//! execution row and only afterwards tears the pane down, so between
//! those two points the execution is observably "terminal with a live
//! pane" — precisely [`crate::terminal_work_sweep`]'s reap precondition.
//! When the teardown sat behind an unbounded `cube workspace release`
//! subprocess (observed at 388 s), the sweep pre-empted it and SIGKILLed
//! a worker that was still running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tempfile::tempdir;

use super::*;
use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::teardown_registry::TeardownRegistry;
use crate::terminal_work_sweep::{self, WorkerReaper};

/// Ordered log of the teardown side effects a completion path performs,
/// shared by the pane releaser, the cube client and the sweep's reaper so
/// a test can assert on their relative order as well as on each having
/// happened.
type TeardownLog = Arc<std::sync::Mutex<Vec<String>>>;

fn log_step(log: &TeardownLog, step: &str) {
    log.lock().expect("teardown log mutex poisoned").push(step.to_owned());
}

fn steps(log: &TeardownLog) -> Vec<String> {
    log.lock().expect("teardown log mutex poisoned").clone()
}

/// A gate a stub can park on until the test opens it, plus a flag the test
/// polls to know the stub has actually reached it.
#[derive(Default)]
struct Gate {
    entered: AtomicBool,
    open: tokio::sync::Notify,
}

impl Gate {
    async fn wait(&self) {
        self.entered.store(true, Ordering::SeqCst);
        self.open.notified().await;
    }

    fn reached(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.open.notify_one();
    }
}

/// `CubeClient` whose `release_workspace` parks on `gate` — models the
/// multi-hundred-second `cube workspace release` from the incident window
/// without making the test sleep.
struct BlockingCubeClient {
    log: TeardownLog,
    gate: Arc<Gate>,
}

crate::stub_cube_client! { BlockingCubeClient {
    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        log_step(&self.log, &format!("cube_release_enter:{lease_id}"));
        self.gate.wait().await;
        log_step(&self.log, &format!("cube_release_exit:{lease_id}"));
        Ok(())
    }
    async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
        Ok(())
    }
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        Ok(Vec::new())
    }
    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        Ok(Vec::new())
    }
} }

/// Pane releaser that models the real teardown closely enough for the
/// sweep to see the difference: it drops the run's
/// `LiveWorkerStateRegistry` entry, which is exactly what makes the slot
/// invisible to `terminal_work_sweep` afterwards. An optional gate lets a
/// test park the completion path *inside* the pane release, with the pane
/// still live — the window the teardown mark exists to protect.
struct LiveStatePaneReleaser {
    log: TeardownLog,
    live_states: Arc<LiveWorkerStateRegistry>,
    gate: Option<Arc<Gate>>,
}

#[async_trait]
impl WorkerPaneReleaser for LiveStatePaneReleaser {
    async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
        log_step(&self.log, &format!("release_pane_enter:{run_id}"));
        if let Some(gate) = &self.gate {
            gate.wait().await;
        }
        if let Some(slot) = self
            .live_states
            .snapshot()
            .into_iter()
            .find(|s| s.run_id == run_id)
            .map(|s| s.slot_id)
        {
            self.live_states.release_slot(slot);
        }
        log_step(&self.log, &format!("release_pane_exit:{run_id}"));
        PaneReleaseOutcome::Reaped
    }
}

/// Reaper stub for the sweep: records what it was asked to reap and, like
/// the production `release_worker_pane`, drops the live-state entry.
struct RecordingSweepReaper {
    log: TeardownLog,
    live_states: Arc<LiveWorkerStateRegistry>,
    reaped: std::sync::Mutex<Vec<String>>,
}

impl RecordingSweepReaper {
    fn new(log: TeardownLog, live_states: Arc<LiveWorkerStateRegistry>) -> Self {
        Self {
            log,
            live_states,
            reaped: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn reaped(&self) -> Vec<String> {
        self.reaped.lock().expect("reaped mutex poisoned").clone()
    }
}

#[async_trait]
impl WorkerReaper for RecordingSweepReaper {
    async fn reap_terminal_worker(&self, run_id: &str) {
        log_step(&self.log, &format!("sweep_reap:{run_id}"));
        self.reaped
            .lock()
            .expect("reaped mutex poisoned")
            .push(run_id.to_owned());
        if let Some(slot) = self
            .live_states
            .snapshot()
            .into_iter()
            .find(|s| s.run_id == run_id)
            .map(|s| s.slot_id)
        {
            self.live_states.release_slot(slot);
        }
    }
}

/// `CubeClient` for the sweep side — it only ever force-releases.
#[derive(Default)]
struct SweepCubeClient;

crate::stub_cube_client! { SweepCubeClient {
    async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
        Ok(())
    }
} }

/// Register a live worker pane bound to `execution_id`.
fn register_live_worker(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, work_item_id: &str) {
    live_states.register_spawn(
        slot_id,
        execution_id,
        "claude-opus-5",
        std::process::id() as i32,
        Some(boss_protocol::WorkItemBinding {
            work_item_id: work_item_id.to_owned(),
            work_item_name: "teardown race".to_owned(),
            execution_id: execution_id.to_owned(),
        }),
    );
}

/// Yield until `predicate` holds, or panic after a bounded number of
/// polls. Used to observe the spawned completion task reaching a gate
/// without sleeping for a fixed duration.
async fn await_condition(label: &str, predicate: impl Fn() -> bool) {
    for _ in 0..2_000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("timed out waiting for: {label}");
}

/// Run a full two-pass-plus sweep cycle and assert it reaped nothing.
async fn assert_sweep_reaps_nothing(
    db: &Arc<WorkDb>,
    live_states: &Arc<LiveWorkerStateRegistry>,
    reaper: &RecordingSweepReaper,
    teardown_registry: &TeardownRegistry,
    sink: &Arc<RecordingDispatchEventSink>,
    context: &str,
) {
    let sweep_cube = SweepCubeClient;
    let mut seen = std::collections::HashMap::new();
    for pass in 1..=3 {
        let outcome = terminal_work_sweep::run_one_pass(
            db.as_ref(),
            live_states,
            reaper,
            &sweep_cube,
            sink.as_ref(),
            teardown_registry,
            &mut seen,
            terminal_work_sweep::DEFAULT_INTERVAL,
        )
        .await;
        assert_eq!(
            outcome.reaped, 0,
            "{context}, pass {pass}: the sweep must not reap a pane whose own completion teardown is in flight",
        );
    }
    assert!(
        reaper.reaped().is_empty(),
        "{context}: no reap may be attempted during the teardown window; got {:?}",
        reaper.reaped(),
    );
    assert!(
        sink.events().await.is_empty(),
        "{context}: no terminal-work reconcile event may be emitted during the teardown window",
    );
}

/// **The incident regression.** A worker's completion teardown is in
/// flight — the execution row is already terminal, and the pane is still
/// live because `release_pane` has not finished — while
/// `terminal_work_sweep` runs a full confirmation cycle. The sweep must
/// reap nothing: this pane belongs to its own teardown, not to a strand.
///
/// The block is placed inside `release_pane` deliberately. That is the
/// only remaining await in the guarded window now that the cube release
/// has moved behind it, so it is what actually exercises the teardown
/// mark rather than the reordering.
#[tokio::test]
async fn sweep_does_not_reap_a_worker_whose_pane_teardown_is_in_flight() {
    let logs = crate::test_support::log_capture::install();
    let log_offset = logs.lock().len();

    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_live_worker(&live_states, 3, &execution_id, &chore_id);

    let log: TeardownLog = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pane_gate = Arc::new(Gate::default());
    let cube_gate = Arc::new(Gate::default());
    cube_gate.release();
    let cube = Arc::new(BlockingCubeClient {
        log: log.clone(),
        gate: cube_gate,
    });
    let pane = Arc::new(LiveStatePaneReleaser {
        log: log.clone(),
        live_states: live_states.clone(),
        gate: Some(pane_gate.clone()),
    });
    // ONE registry, shared by the handler and the sweep — the production
    // wiring in `app.rs`. Two instances would protect nothing.
    let teardown_registry = Arc::new(TeardownRegistry::new());
    let handler = Arc::new(
        WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            Arc::new(RecordingPublisher::default()),
            pane.clone(),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_teardown_registry(teardown_registry.clone()),
    );

    // Drive the Stop-hook PR-completion path. `Done` (merged) is used so
    // the reviewer-enqueue branch — which would reach for `gh` — is not
    // taken; the teardown sequence under test is identical on both arms.
    let completion = tokio::spawn({
        let handler = handler.clone();
        let execution_id = execution_id.clone();
        async move {
            handler
                .finalize_pr_transition(
                    &execution_id,
                    "https://github.com/spinyfin/mono/pull/9100".to_owned(),
                    WorkerPrCompletionTarget::Done,
                    "test",
                )
                .await
        }
    });

    // Wait until the completion path is parked mid-teardown with its pane
    // still live — the exact state the incident trace captured.
    let gate = pane_gate.clone();
    await_condition("pane teardown entered", || gate.reached()).await;
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        boss_protocol::ExecutionStatus::Completed,
        "precondition: the execution must already be terminal while its teardown is still running",
    );

    let reaper = RecordingSweepReaper::new(log.clone(), live_states.clone());
    let sink = Arc::new(RecordingDispatchEventSink::new());
    assert_sweep_reaps_nothing(
        &db,
        &live_states,
        &reaper,
        teardown_registry.as_ref(),
        &sink,
        "teardown in flight",
    )
    .await;

    // The skip must be attributable, not silent.
    let captured = String::from_utf8_lossy(&logs.lock()[log_offset..]).into_owned();
    assert!(
        captured
            .lines()
            .any(|l| l.contains(&execution_id) && l.contains("own completion teardown is still")),
        "the sweep must say why it skipped; captured:\n{captured}",
    );

    // Let the teardown finish; the completion path owns the pane teardown
    // and must be the one that performs it.
    pane_gate.release();
    completion.await.unwrap();

    let steps = steps(&log);
    assert!(
        steps.iter().any(|s| s == &format!("release_pane_exit:{execution_id}")),
        "the completion path must still release the pane itself; steps: {steps:?}",
    );
    assert!(
        !steps.iter().any(|s| s.starts_with("sweep_reap:")),
        "the sweep must never have been the one to reap this pane; steps: {steps:?}",
    );
    assert_eq!(
        teardown_registry.state(&execution_id, std::time::Instant::now()),
        crate::teardown_registry::TeardownState::Idle,
        "the mark must clear once teardown finishes, so a later strand is still reclaimable",
    );
}

/// The original incident shape: the block is the `cube workspace release`
/// itself. Post-fix this is safe for two independent reasons — the cube
/// call now runs after the pane is already gone, AND the teardown mark
/// still covers it — so a slow cube can no longer cost a live worker.
#[tokio::test]
async fn sweep_does_not_reap_a_worker_behind_a_slow_cube_release() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_live_worker(&live_states, 5, &execution_id, &chore_id);

    let log: TeardownLog = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cube_gate = Arc::new(Gate::default());
    let cube = Arc::new(BlockingCubeClient {
        log: log.clone(),
        gate: cube_gate.clone(),
    });
    let pane = Arc::new(LiveStatePaneReleaser {
        log: log.clone(),
        live_states: live_states.clone(),
        gate: None,
    });
    let teardown_registry = Arc::new(TeardownRegistry::new());
    let handler = Arc::new(
        WorkerCompletionHandler::new(
            db.clone(),
            StubPrDetector::ok(None),
            cube.clone(),
            Arc::new(RecordingPublisher::default()),
            pane.clone(),
            Arc::new(RecordingProbeQueuer::default()),
        )
        .with_teardown_registry(teardown_registry.clone()),
    );

    let completion = tokio::spawn({
        let handler = handler.clone();
        let execution_id = execution_id.clone();
        async move {
            handler
                .finalize_pr_transition(
                    &execution_id,
                    "https://github.com/spinyfin/mono/pull/9101".to_owned(),
                    WorkerPrCompletionTarget::Done,
                    "test",
                )
                .await
        }
    });

    let gate = cube_gate.clone();
    await_condition("cube release entered", || gate.reached()).await;

    let reaper = RecordingSweepReaper::new(log.clone(), live_states.clone());
    let sink = Arc::new(RecordingDispatchEventSink::new());
    assert_sweep_reaps_nothing(
        &db,
        &live_states,
        &reaper,
        teardown_registry.as_ref(),
        &sink,
        "slow cube release",
    )
    .await;

    cube_gate.release();
    completion.await.unwrap();

    let steps = steps(&log);
    let pane_at = steps
        .iter()
        .position(|s| s == &format!("release_pane_exit:{execution_id}"))
        .expect("the pane must be released");
    let cube_at = steps
        .iter()
        .position(|s| s.starts_with("cube_release_enter:"))
        .expect("the cube workspace lease must be released");
    assert!(
        pane_at < cube_at,
        "release_pane must complete before the unbounded cube release even starts; steps: {steps:?}",
    );
}

/// The full teardown ordering the incident scrambled, asserted in one
/// place: pane, then driver state (never while the process may still be
/// alive), then the cube workspace.
///
/// Single-threaded runtime: `driver_teardown`'s test hook counts calls in
/// a thread-local.
#[tokio::test]
async fn pr_completion_teardown_runs_pane_then_driver_then_cube() {
    let workspace = tempdir().unwrap();
    let (_dir, db, _product_id, chore_id, execution_id) = fixture(workspace.path());

    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    register_live_worker(&live_states, 7, &execution_id, &chore_id);

    let log: TeardownLog = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cube_gate = Arc::new(Gate::default());
    cube_gate.release();
    let cube = Arc::new(BlockingCubeClient {
        log: log.clone(),
        gate: cube_gate,
    });
    let pane = Arc::new(LiveStatePaneReleaser {
        log: log.clone(),
        live_states: live_states.clone(),
        gate: None,
    });
    let handler = WorkerCompletionHandler::new(
        db.clone(),
        StubPrDetector::ok(None),
        cube.clone(),
        Arc::new(RecordingPublisher::default()),
        pane.clone(),
        Arc::new(RecordingProbeQueuer::default()),
    );

    crate::driver_teardown::test_hooks::reset();
    handler
        .finalize_pr_transition(
            &execution_id,
            "https://github.com/spinyfin/mono/pull/9102".to_owned(),
            WorkerPrCompletionTarget::Done,
            "test",
        )
        .await;

    assert_eq!(
        crate::driver_teardown::test_hooks::count(),
        1,
        "driver teardown must run exactly once on this termination path",
    );
    let steps = steps(&log);
    assert_eq!(
        steps,
        vec![
            format!("release_pane_enter:{execution_id}"),
            format!("release_pane_exit:{execution_id}"),
            "cube_release_enter:lease-1".to_owned(),
            "cube_release_exit:lease-1".to_owned(),
        ],
        "pane release must precede the cube release",
    );
}
