//! End-to-end reproduction of the 2026-07-30 "pane hosts a login shell,
//! driver never started" failure, driven against a **real OS process**
//! rather than a fabricated pid.
//!
//! ## Why this exists separately from the unit tests
//!
//! The unit tests in `spawn_ack_sweep.rs` and `live_worker_state.rs` pin
//! each predicate in isolation. What they cannot show is the property that
//! actually made the incident invisible: that *all three* of Boss's
//! existing safety nets independently pass the slot, each for its own
//! reason, and that they do so against a genuinely live process — not a
//! number chosen by a test.
//!
//! So this module spawns a real long-lived child, registers it as the
//! pane's foreground shell the way production stamps `shell_pid` at
//! spawn-registration time, and then runs the **real** sweeps in sequence:
//!
//! 1. [`crate::dead_pid_sweep`] — `kill(pid, 0)` finds the child alive, so
//!    it declines to reap. `dead_pid_sweep` is the nominal fallback for
//!    grok-class drivers, and is insufficient on its own.
//! 2. [`crate::live_worker_state::LiveWorkerStateRegistry::mark_stalled_spawns`]
//!    — declines to promote, because grok omits
//!    `Capability::AwaitingInputSignal`.
//! 3. [`crate::spawn_ack_sweep`]'s tmux-invariant check — declines. A
//!    reported pid is not a pid-less `Spawning` slot. On a re-adopted slot
//!    the invariant also declines because re-adoption did not create a new
//!    pane awaiting acknowledgement.
//!
//! Then pass 2 fires and the resources are actually returned. The point of
//! asserting steps 1–3 rather than only step 4 is that a future change
//! which "fixes" this by loosening one of those three would be silently
//! reintroducing the false positives each guard exists to prevent — this
//! test makes that visible. Two `#[tokio::test]`s below share the same
//! fixture, one for each pass-1 decline shape.

use std::sync::Arc;

use async_trait::async_trait;
use boss_protocol::{WorkItemBinding, WorkerActivity};

use crate::dispatch_events::RecordingDispatchEventSink;
use crate::live_worker_state::{
    DRIVER_START_GRACE_SECS, LiveSpawnRouting, LiveWorkerStateRegistry, ReadoptionEvidence,
    STALLED_SPAWN_THRESHOLD_SECS,
};
use crate::spawn_ack_sweep::{DRIVER_START_ATTENTION_KIND, SpawnAckReaper, run_one_pass};
use crate::spawn_health::SpawnHealthTracker;
use crate::test_support::*;
use crate::work::ExecutionStatus;

/// A real child process standing in for the pane's idle login shell.
/// Killed on drop so a failing assertion cannot leak it.
struct LiveShell(std::process::Child);

impl LiveShell {
    fn spawn() -> Self {
        // Long enough that it cannot exit mid-test and turn this into an
        // accidental dead-pid test.
        let child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .expect("failed to spawn the stand-in login shell");
        Self(child)
    }

    fn pid(&self) -> i32 {
        self.0.id() as i32
    }
}

impl Drop for LiveShell {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Reaper double that records the pid it was asked to tear down. In
/// production this is `ServerState::release_worker_pane`, which signals the
/// recorded shell pid's process group — the mechanism that stops a reaped
/// slot leaving behind the live-process-under-an-orphaned-row state that
/// `retire_pane`'s durable-liveness guard refuses to touch.
struct RecordingReaper {
    reaped: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl SpawnAckReaper for RecordingReaper {
    async fn reap_worker(&self, execution_id: &str) {
        self.reaped.lock().unwrap().push(execution_id.to_owned());
    }
}

crate::stub_cube_client! { RecordingCube {
    async fn force_release_lease(&self, lease_id: &str, _reason: Option<&str>) -> anyhow::Result<()> {
        self.released.lock().unwrap().push(lease_id.to_owned());
        Ok(())
    }
} }

#[derive(Default)]
struct RecordingCube {
    released: std::sync::Mutex<Vec<String>>,
}

/// Common fixture for both real-process incident reproductions below: a
/// live child process standing in for the pane's login shell, a matching
/// DB row, and a `Spawning`, grok-modeled live-state entry with an aged
/// `spawned_at` and no driver-originated signal.
///
/// `readopted` selects whether the slot is restored the way tmux's
/// periodic convergence pass would (`true`) or left as this engine
/// spawned it (`false`) — the original 2026-07-30 shape. Both must reach
/// driver-start verification.
struct IncidentFixture {
    shell: LiveShell,
    db: Arc<crate::work::WorkDb>,
    execution_id: String,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<crate::coordinator::ExecutionCoordinator>,
}

fn setup_incident(readopted: bool) -> IncidentFixture {
    let shell = LiveShell::spawn();
    let shell_pid = shell.pid();

    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "chore whose driver never starts");
    let db = Arc::new(db);

    // The DB shape a real pane spawn leaves behind: a started run on the
    // local host with the app-reported shell pid recorded, the execution
    // parked at `waiting_human`, and a cube workspace lease held.
    let execution_id = create_spawned_execution(&db, &work_item_id, shell_pid as i64);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "precondition: the incident's execution is a live pane-hosted worker (`running`)",
    );

    // The live-state shape: grok (no `Capability::AwaitingInputSignal`),
    // still `Spawning`, with the login shell's pid reported by the app and
    // no driver-originated signal of any kind.
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    live_states.register_spawn_with_capabilities(
        1,
        &execution_id,
        "grok-4.7",
        shell_pid,
        Some(WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "chore whose driver never starts".to_owned(),
            execution_id: execution_id.clone(),
        }),
        false,
        LiveSpawnRouting::none(),
    );
    // Age `spawned_at` *before* re-adoption, matching the incident: this is
    // periodic same-run adoption of a slot whose original spawn is already
    // stale, not a fresh spawn immediately re-adopted. Ageing afterward
    // would mask a regression where the retain path started refreshing
    // `spawned_at` to "now" on re-adoption — the artificial ageing call
    // would silently overwrite that refresh and the test would stay green
    // even though production would have an immortal shell-only pane.
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    live_states.set_spawn_time_for_test(1, now - (DRIVER_START_GRACE_SECS + 60));

    if readopted {
        // Tmux adoption observes the same live login shell on its periodic
        // convergence pass. That must not turn a shell-only observation
        // into a permanent driver-start exemption, and must not refresh
        // `spawned_at`.
        live_states.register_readoption(
            1,
            &execution_id,
            "grok-4.7",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: "chore whose driver never starts".to_owned(),
                execution_id: execution_id.clone(),
            }),
            false,
            LiveSpawnRouting::none(),
            ReadoptionEvidence::LiveShellPid,
        );
    }

    let post_setup = live_states.unverified_driver_starts(now, 0);
    assert_eq!(post_setup.len(), 1, "the slot must be silent after setup");
    assert!(
        post_setup[0].silent_secs >= DRIVER_START_GRACE_SECS + 60,
        "re-adoption must not refresh spawned_at for an already-aged slot (silent_secs={}); \
         refreshing it would reset the grace window and mask a periodic same-run adoption of an \
         already-stale spawn",
        post_setup[0].silent_secs,
    );

    let coordinator = make_coordinator(db.clone(), 1);
    IncidentFixture {
        shell,
        db,
        execution_id,
        live_states,
        coordinator,
    }
}

/// Runs gates 1-3 plus the fix against `fixture`, asserting the common
/// downstream outcome (reap, lease release, attention item, dispatch
/// event).
async fn assert_incident_detected_and_released(fixture: &IncidentFixture) {
    let IncidentFixture {
        shell,
        db,
        execution_id,
        live_states,
        coordinator,
    } = fixture;
    let shell_pid = shell.pid();
    let now = boss_engine_utils::epoch_time::now_epoch_secs();

    coordinator.worker_pool().claim_worker(execution_id, None).await;
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(execution_id),
        "precondition: the worker slot is held",
    );

    let _bookmark_store = crate::test_support::seed_empty_execution_bookmark(db, execution_id).await;

    // ── Gate 1: dead_pid_sweep. `kill(pid, 0)` finds the shell alive. ────
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let dead_pid_outcome =
        crate::dead_pid_sweep::run_one_pass(db.as_ref(), live_states, coordinator.clone(), sink.as_ref(), &NoopCube)
            .await;
    assert_eq!(
        dead_pid_outcome.reaped, 0,
        "gate 1: the login shell is genuinely alive, so kill(pid, 0) cannot detect this",
    );

    // ── Gate 2: mark_stalled_spawns. Grok is exempt. ─────────────────────
    let promoted = live_states.mark_stalled_spawns(now, STALLED_SPAWN_THRESHOLD_SECS);
    assert!(
        promoted.is_empty(),
        "gate 2: a driver without Capability::AwaitingInputSignal is not promoted",
    );
    assert_eq!(
        live_states.get(1).unwrap().activity,
        WorkerActivity::Spawning,
        "gate 2: the slot is left sitting in Spawning — the incident's observed state",
    );

    // ── Gate 3 + the fix: spawn_ack_sweep. ───────────────────────────────
    // The tmux-invariant check declines a reported pid; driver-start fires.
    let reaper = Arc::new(RecordingReaper {
        reaped: std::sync::Mutex::new(Vec::new()),
    });
    let cube = RecordingCube::default();
    let spawn_health = SpawnHealthTracker::new();
    let outcome = run_one_pass(
        db.as_ref(),
        live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(
        outcome.tmux_invariant_pidless, 0,
        "a reported pid is not a tmux-invariant pid-less slot",
    );

    // ── Detected, attention raised, both resources freed.
    assert_eq!(
        outcome.driver_start_reaped, 1,
        "driver-start verification must detect the never-started driver",
    );

    assert_eq!(
        db.get_execution(execution_id).unwrap().status,
        ExecutionStatus::Orphaned,
        "the execution must be terminalized rather than parked forever",
    );

    assert!(
        !coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(execution_id),
        "the worker slot must be released",
    );

    assert_eq!(
        *cube.released.lock().unwrap(),
        vec!["lease-1".to_owned()],
        "the cube workspace lease must be released",
    );

    assert_eq!(
        *reaper.reaped.lock().unwrap(),
        vec![execution_id.clone()],
        "the pane must be torn down, which is what signals the live shell's process group and \
         stops a live pid being left under an orphaned row",
    );

    let attentions = db.list_attention_items(execution_id).unwrap();
    assert_eq!(attentions.len(), 1, "an attention item must be raised");
    assert_eq!(attentions[0].kind, DRIVER_START_ATTENTION_KIND);
    assert!(
        attentions[0].body_markdown.contains(&shell_pid.to_string()),
        "the attention item must name the pid that misled every other check",
    );

    let events = sink.events().await;
    assert_eq!(
        events.iter().filter(|e| e.stage == "driver_start_timeout").count(),
        1,
        "exactly one driver_start_timeout dispatch event",
    );
}

/// The full incident, start to finish, on a re-adopted slot.
#[tokio::test]
async fn a_pane_hosting_only_a_live_login_shell_is_detected_and_fully_released() {
    let fixture = setup_incident(true);
    assert_incident_detected_and_released(&fixture).await;
}

/// The same incident against a pane this engine spawned itself — never
/// re-adopted. This is the original 2026-07-30 reproduction shape.
#[tokio::test]
async fn an_engine_spawned_pane_hosting_only_a_live_login_shell_is_detected_and_fully_released() {
    let fixture = setup_incident(false);
    assert_incident_detected_and_released(&fixture).await;
}

/// The control: the identical setup, with the one difference that the
/// driver signalled. Nothing is touched.
///
/// The detection must not reap a working worker, and in particular a live
/// shell pid is not what distinguishes the two cases. Both runs here have
/// one; only the driver signal differs.
#[tokio::test]
async fn the_same_pane_with_a_driver_signal_is_left_completely_alone() {
    let shell = LiveShell::spawn();
    let shell_pid = shell.pid();

    let (_dir, db) = open_db();
    let product_id = create_product(&db);
    let work_item_id = create_active_chore(&db, &product_id, "chore whose driver works");
    let db = Arc::new(db);

    let execution_id = create_spawned_execution(&db, &work_item_id, shell_pid as i64);
    let live_states = Arc::new(LiveWorkerStateRegistry::new());
    live_states.register_spawn_with_capabilities(
        1,
        &execution_id,
        "grok-4.7",
        shell_pid,
        Some(WorkItemBinding {
            work_item_id: work_item_id.clone(),
            work_item_name: "chore whose driver works".to_owned(),
            execution_id: execution_id.clone(),
        }),
        false,
        LiveSpawnRouting::none(),
    );
    let now = boss_engine_utils::epoch_time::now_epoch_secs();
    live_states.set_spawn_time_for_test(1, now - (DRIVER_START_GRACE_SECS + 60));

    // The only difference from the test above.
    live_states.record_driver_signal(&execution_id, crate::live_worker_state::DriverSignalKind::HookEvent);

    let coordinator = make_coordinator(db.clone(), 1);
    coordinator.worker_pool().claim_worker(&execution_id, None).await;

    let reaper = Arc::new(RecordingReaper {
        reaped: std::sync::Mutex::new(Vec::new()),
    });
    let cube = RecordingCube::default();
    let sink = Arc::new(RecordingDispatchEventSink::new());
    let spawn_health = SpawnHealthTracker::new();
    let outcome = run_one_pass(
        db.as_ref(),
        &live_states,
        coordinator.clone(),
        sink.as_ref(),
        reaper.as_ref(),
        &spawn_health,
        &cube,
        DRIVER_START_GRACE_SECS,
    )
    .await;

    assert_eq!(outcome.driver_start_reaped, 0);
    assert_eq!(
        db.get_execution(&execution_id).unwrap().status,
        ExecutionStatus::Running,
        "a working worker's execution must be untouched",
    );
    assert!(
        coordinator
            .worker_pool()
            .claimed_execution_ids()
            .await
            .contains(&execution_id),
        "a working worker must keep its slot",
    );
    assert!(
        cube.released.lock().unwrap().is_empty(),
        "a working worker must keep its cube lease",
    );
    assert!(
        reaper.reaped.lock().unwrap().is_empty(),
        "its pane must not be torn down"
    );
    assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
}
