//! Periodic reconciler that detects worker slots whose `SpawnWorkerPane`
//! RPC returned `ok` but no evidence a `claude` session ever actually
//! started exists — the "false-live" spawn.
//!
//! ## The gap this closes
//!
//! `spawn_flow::start_worker` treats a successful `SpawnWorkerPane`
//! response from the app as "the worker is up": it emits a
//! [`crate::dispatch_events::Stage::PaneSpawned`] `ok` event and stamps a
//! `Spawning` [`crate::live_worker_state::LiveWorkerState`], all before any
//! evidence the app actually produced a live `claude` process exists. The
//! app's shell pid arrives asynchronously (`onSurfaceAttached` →
//! `update_worker_shell_pid`), and the worker's own hook events only start
//! flowing once `claude` itself launches. If the pane's shell never came
//! up — a wedged libghostty surface, a dead pty, an app-side spawn that
//! silently no-op'd — neither of those ever arrives: `shell_pid` stays `0`
//! and no hook event is ever received.
//!
//! Before this sweep existed,
//! [`crate::live_worker_state::LiveWorkerStateRegistry::mark_stalled_spawns`]
//! actively hid this failure: any slot stuck in `Spawning` with no hook
//! event for [`crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS`] was
//! promoted to `WaitingForInput` — a heuristic that exists to surface the
//! *legitimate* case where `claude`'s initial directory-trust prompt blocks
//! the run before `SessionStart` fires. That heuristic cannot distinguish
//! "parked at a real prompt in a real session" from "no session ever
//! started": both look identical from the no-hook-event vantage point.
//! `mark_stalled_spawns` now only promotes the `shell_pid > 0` case (the
//! trust-prompt worker's shell has, at minimum, exec'd and become the
//! pty's foreground process group); a slot stuck in `Spawning` with
//! `shell_pid <= 0` is left alone for this sweep to catch as a genuine
//! failure instead of being mislabelled "waiting for input" forever — the
//! false-live incident (`shell_pid: 0`, `activity: waiting_for_input`
//! indefinitely, no hook ever received) this sweep exists to fix.
//!
//! ## Algorithm
//!
//! 1. Snapshot [`crate::live_worker_state::LiveWorkerStateRegistry`].
//! 2. For each slot with `activity == Spawning` and `shell_pid <= 0`:
//!    1. Look up the execution in the DB (age guard: skip if
//!       `started_at` is within [`SPAWN_LIVENESS_TIMEOUT_SECS`] seconds or
//!       `None`, mirroring the other sweeps' grace periods).
//!    2. If the execution is still within the window, skip — the pane may
//!       still be spinning up.
//!    3. Past the window with no shell pid and no hook ever observed:
//!       reap the pane (same `release_worker_pane` teardown the
//!       stale-worker sweep uses — the app may still be holding a dead or
//!       wedged pty for this slot), mark the execution `orphaned`, and
//!       release the pool slot so the orphan sweep redispatches.
//! 3. Emit a `spawn_liveness_timeout` dispatch event carrying every signal
//!    actually observed (`shell_pid`, `hook_event_received: false`, age)
//!    so `bossctl dispatch diagnose <exec-id>` answers "did the spawn ever
//!    produce a session" without a log dive.
//!
//! ## Cadence
//!
//! Runs every 30 seconds and fires once immediately on boot (same pattern
//! as [`crate::dead_pid_sweep`]).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_protocol::{WorkItemPatch, WorkerActivity};

use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::stale_worker_sweep::StaleWorkerReaper;
use crate::work::WorkDb;

/// How long a slot may sit in `Spawning` with `shell_pid <= 0` and no hook
/// event before this sweep treats it as a confirmed dead spawn rather than
/// a pane still coming up. Deliberately well above
/// [`crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS`] (30s): by the
/// time this fires, a *legitimate* trust-prompt stall would already have
/// reported a real shell pid and been promoted to `WaitingForInput` by
/// `mark_stalled_spawns`. A slot that still has no pid at all after this
/// long was never going to get one.
pub const SPAWN_LIVENESS_TIMEOUT_SECS: i64 = 90;

/// Counts from one pass of the sweep; logged at `info` when activity
/// occurs.
#[derive(Debug, Default)]
pub struct SpawnLivenessSweepOutcome {
    pub reaped: usize,
    pub shell_pid_present_skipped: usize,
    pub not_spawning_skipped: usize,
    pub grace_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for SpawnLivenessSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            grace_skipped = self.grace_skipped,
            "spawn-liveness sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    reaper: Arc<dyn StaleWorkerReaper>,
    interval: Duration,
    timeout_secs: i64,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let reaper = Arc::clone(&reaper);
        async move {
            run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                reaper.as_ref(),
                timeout_secs,
            )
            .await
        }
    })
}

/// Run a single spawn-liveness sweep pass. Returns a summary of what
/// happened; callers may log it.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    reaper: &dyn StaleWorkerReaper,
    timeout_secs: i64,
) -> SpawnLivenessSweepOutcome {
    let mut outcome = SpawnLivenessSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let grace_cutoff = now_epoch_secs - timeout_secs;

    for state in snapshot {
        // Only slots still `Spawning` with no shell pid are candidates.
        // `mark_stalled_spawns` never promotes this case (see module
        // docs), so a real trust-prompt stall (shell_pid > 0) has already
        // moved to `WaitingForInput` by the time this sweep would
        // otherwise consider it.
        if state.activity != WorkerActivity::Spawning {
            outcome.not_spawning_skipped += 1;
            continue;
        }
        if state.shell_pid > 0 {
            outcome.shell_pid_present_skipped += 1;
            continue;
        }

        let execution_id = &state.run_id;

        let execution = match work_db.get_execution(execution_id) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "spawn-liveness sweep: failed to look up execution; skipping slot",
                );
                continue;
            }
        };

        // Skip executions already terminal (completion path may have
        // raced the sweep).
        if execution.status.is_terminal() {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within the timeout window or not yet recorded — the pane may
        // still be spinning up.
        let started_epoch = execution.started_at.as_deref().and_then(|s| s.parse::<i64>().ok());
        match started_epoch {
            None => {
                outcome.grace_skipped += 1;
                continue;
            }
            Some(t) if t >= grace_cutoff => {
                outcome.grace_skipped += 1;
                continue;
            }
            _ => {}
        }
        let started_epoch = started_epoch.expect("checked above");

        tracing::warn!(
            execution_id,
            work_item_id = %execution.work_item_id,
            slot_id = state.slot_id,
            shell_pid = state.shell_pid,
            seconds_since_started = now_epoch_secs - started_epoch,
            "spawn-liveness sweep: SpawnWorkerPane reported ok but no session ever \
             started — no shell pid and no hook event within the liveness window; \
             reaping and redispatching",
        );

        let reason = format!(
            "spawn-liveness-timeout: SpawnWorkerPane returned ok but shell_pid stayed 0 and \
             no hook event was received within {timeout_secs}s; no claude session ever started"
        );
        if let Err(err) = work_db.mark_execution_orphaned(execution_id, &reason) {
            tracing::warn!(
                execution_id,
                ?err,
                "spawn-liveness sweep: failed to mark execution orphaned; skipping reap",
            );
            continue;
        }

        // A spawn that never produced a session has no meaningful
        // workspace changes to back up, but the workspace may still hold
        // a wedged pane on the app side (a real pty that never got a
        // shell pid reported, or a surface libghostty is stuck
        // initializing) — reap it the same way the stale-worker sweep
        // does, before the slot/lease is freed for redispatch.
        reaper.reap_worker(execution_id).await;

        if let Err(err) = append_reconcile_audit(
            work_db,
            &execution.work_item_id,
            execution_id,
            now_epoch_secs,
            state.shell_pid,
            timeout_secs,
        ) {
            tracing::warn!(
                work_item_id = %execution.work_item_id,
                ?err,
                "spawn-liveness sweep: failed to append audit line to description (non-fatal)",
            );
        }

        let worker_id = worker_id_for_slot(state.slot_id);
        coordinator.release_worker_and_kick(&worker_id, None).await;

        dispatch_events
            .emit(
                DispatchEvent::new(Stage::SpawnLivenessTimeout, Outcome::Ok, execution_id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "slot_id": state.slot_id,
                        "shell_pid": state.shell_pid,
                        "hook_event_received": false,
                        "seconds_since_started": now_epoch_secs - started_epoch,
                        "timeout_secs": timeout_secs,
                    })),
            )
            .await;

        outcome.reaped += 1;
    }

    outcome
}

/// Append an `[engine-reconcile]` audit line to the work item's
/// description so an operator can see why the chore was reset, and what
/// the sweep actually observed at reap time.
fn append_reconcile_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    dead_execution_id: &str,
    now_epoch_secs: i64,
    shell_pid: i32,
    timeout_secs: i64,
) -> anyhow::Result<()> {
    let item = work_db.get_work_item(work_item_id)?;
    let current_desc = match &item {
        boss_protocol::WorkItem::Product(p) => p.description.as_str(),
        boss_protocol::WorkItem::Project(p) => p.description.as_str(),
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.description.as_str(),
    };
    let audit_line = format!(
        "\n[engine-reconcile] epoch {now_epoch_secs}: spawn-liveness timeout (exec {dead_execution_id}): \
         SpawnWorkerPane returned ok but shell_pid stayed {shell_pid} and no hook event arrived within \
         {timeout_secs}s; no claude session ever started. Chore reset to todo for redispatch."
    );
    let new_desc = format!("{current_desc}{audit_line}");
    work_db.update_work_item(
        work_item_id,
        WorkItemPatch {
            description: Some(new_desc),
            ..WorkItemPatch::default()
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::WorkItemBinding;
    use tempfile::TempDir;

    use super::*;
    use crate::coordinator::{
        CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
        ExecutionCoordinator, WorkerPool,
    };
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::runner::{ExecutionRunner, RunOutcome};
    use crate::test_support::*;
    use crate::work::{CreateChoreInput, ExecutionStatus, WorkDb, WorkItemPatch};
    use boss_protocol::WorkExecution;

    struct NoopCube;

    #[async_trait]
    impl CubeClient for NoopCube {
        async fn ensure_repo(&self, _: &str) -> Result<CubeRepoHandle> {
            unimplemented!()
        }
        async fn lease_workspace(
            &self,
            _: &str,
            _: &str,
            _: Option<&str>,
            _: bool,
            _: &[&str],
        ) -> Result<CubeWorkspaceLease> {
            unimplemented!()
        }
        async fn create_change(&self, _: &std::path::Path, _: &str) -> Result<CubeChangeHandle> {
            unimplemented!()
        }
        async fn goto_workspace(&self, _: &std::path::Path, _: u64) -> Result<()> {
            unimplemented!()
        }
        async fn release_workspace(&self, _: &str) -> Result<()> {
            unimplemented!()
        }
        async fn workspace_status(&self, _: &std::path::Path) -> Result<CubeWorkspaceStatus> {
            unimplemented!()
        }
        async fn heartbeat_lease(&self, _: &str, _: Option<u64>) -> Result<()> {
            unimplemented!()
        }
        async fn force_release_lease(&self, _: &str, _: Option<&str>) -> Result<()> {
            unimplemented!()
        }
        async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
            Ok(vec![])
        }
        async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
            Ok(vec![])
        }
    }

    struct NoopRunner;

    #[async_trait]
    impl ExecutionRunner for NoopRunner {
        async fn run_execution(
            &self,
            _worker_id: &str,
            _execution: &WorkExecution,
            _work_item: &crate::work::WorkItem,
            _workspace_path: &std::path::Path,
            _cube_change_id: Option<&str>,
        ) -> Result<RunOutcome> {
            unimplemented!()
        }
    }

    /// Records every `reap_worker` call so tests can assert the reaper
    /// ran before the pool slot is released.
    #[derive(Default)]
    struct RecordingReaper {
        calls: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl StaleWorkerReaper for RecordingReaper {
        async fn reap_worker(&self, execution_id: &str) {
            self.calls.lock().unwrap().push(execution_id.to_owned());
        }
    }

    fn open_db() -> (TempDir, WorkDb) {
        let dir = TempDir::new().unwrap();
        let db = WorkDb::open(dir.path().join("state.db")).unwrap();
        (dir, db)
    }

    fn create_product(db: &WorkDb) -> String {
        create_test_product_with_repo(db, "test-product", Some("https://github.com/test/repo")).id
    }

    fn create_active_chore(db: &WorkDb, product_id: &str) -> String {
        let chore = db
            .create_chore(
                CreateChoreInput::builder()
                    .product_id(product_id)
                    .name("test chore")
                    .build(),
            )
            .unwrap();
        db.update_work_item(
            &chore.id,
            WorkItemPatch {
                status: Some("active".to_owned()),
                ..Default::default()
            },
        )
        .unwrap();
        chore.id
    }

    /// Create a `ready` execution for `work_item_id` and stamp its
    /// `started_at` to `age_secs` ago.
    fn create_execution_started(db: &WorkDb, work_item_id: &str, age_secs: u64) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(age_secs) as i64;
        db.force_started_at_for_test(&execution.id, started_at).unwrap();
        execution.id
    }

    fn make_coordinator(db: Arc<WorkDb>, pool_size: usize) -> Arc<ExecutionCoordinator> {
        Arc::new(ExecutionCoordinator::new(
            db,
            WorkerPool::new(pool_size),
            Arc::new(NoopCube),
            Arc::new(NoopRunner),
        ))
    }

    /// Register a `Spawning` slot with the given shell pid via
    /// `register_spawn` (activity defaults to `Spawning`).
    fn register_spawning_slot(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        shell_pid: i32,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    /// A slot stuck `Spawning` with `shell_pid <= 0` past the timeout is
    /// reaped: execution goes `orphaned`, the reaper runs, the pool slot
    /// frees, and a `spawn_liveness_timeout` dispatch event is emitted.
    #[tokio::test]
    async fn dead_spawn_past_timeout_is_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_execution_started(&db, &work_item_id, 200);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_spawning_slot(&live_states, 1, &execution_id, 0, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaper = Arc::new(RecordingReaper::default());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_LIVENESS_TIMEOUT_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 1);
        assert_eq!(reaper.calls.lock().unwrap().as_slice(), [execution_id.clone()]);

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Orphaned);

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(!claimed_after.contains(&execution_id));

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "spawn_liveness_timeout");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description.clone(),
            _ => panic!("expected chore"),
        };
        assert!(desc.contains("[engine-reconcile]"));
        assert!(desc.contains("spawn-liveness timeout"));
    }

    /// A slot with a real shell pid — the legitimate trust-prompt stall —
    /// is never touched by this sweep, even well past the timeout.
    #[tokio::test]
    async fn shell_pid_present_is_never_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_execution_started(&db, &work_item_id, 200);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_spawning_slot(&live_states, 1, &execution_id, 42_111, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaper = Arc::new(RecordingReaper::default());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_LIVENESS_TIMEOUT_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.shell_pid_present_skipped, 1);
        assert!(reaper.calls.lock().unwrap().is_empty());

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Ready);
    }

    /// A dead spawn within the grace window (fresh dispatch) is left
    /// alone — the pane may still be coming up.
    #[tokio::test]
    async fn dead_spawn_within_grace_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_execution_started(&db, &work_item_id, 5);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_spawning_slot(&live_states, 1, &execution_id, 0, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaper = Arc::new(RecordingReaper::default());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_LIVENESS_TIMEOUT_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.grace_skipped, 1);
        assert!(reaper.calls.lock().unwrap().is_empty());
    }

    /// A slot whose activity has already progressed past `Spawning`
    /// (e.g. `Working`) is not touched by this sweep regardless of
    /// shell pid.
    #[tokio::test]
    async fn non_spawning_activity_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_execution_started(&db, &work_item_id, 200);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_spawning_slot(&live_states, 1, &execution_id, 0, &work_item_id);
        live_states.apply_event(
            1,
            &boss_protocol::WorkerEvent::PreToolUse {
                session_id: "test-session".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let sink = Arc::new(RecordingDispatchEventSink::new());
        let reaper = Arc::new(RecordingReaper::default());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_LIVENESS_TIMEOUT_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.not_spawning_skipped, 1);
    }
}
