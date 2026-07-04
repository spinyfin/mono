//! Periodic reconciler that detects and reaps worker slots whose spawn
//! never produced any evidence of a real process — the "false-live"
//! failure class from the 2026-07-03/04 incidents.
//!
//! ## The incident this guards against
//!
//! `SpawnWorkerPane` can return `pane_spawned/ok` (the app accepted the
//! slot and started asynchronously creating a libghostty surface) while
//! no `claude` session — and in the worst case no shell at all — ever
//! actually comes up. Three occurrences on the same slot within about
//! 90 minutes on 2026-07-03/04 showed the pattern: `bossctl agents
//! transcript` reported "engine has not yet received a hook event
//! carrying transcript_path" indefinitely, `agents status` showed
//! `shell_pid: 0` forever, and — because [`LiveWorkerStateRegistry::mark_stalled_spawns`]
//! unconditionally promoted any never-hooked `Spawning` slot to
//! `WaitingForInput` (assuming the worker was merely blocked on the
//! interactive directory-trust prompt) — the slot presented as "needs a
//! human" when there was nothing for a human to attach to and answer. A
//! coordinator had to notice and manually reap it each time.
//!
//! [`crate::dead_pid_sweep`] cannot catch this: it only probes slots
//! with `shell_pid > 0` — a slot that never reported a pid has nothing
//! to `kill(pid, 0)` against. [`crate::stale_worker_sweep`] only looks
//! at `activity == Working`. Neither sweep's failure class matches "the
//! app accepted the spawn but no process, and thus no pid and no hook,
//! ever manifested at all."
//!
//! ## Algorithm
//!
//! 1. Snapshot [`LiveWorkerStateRegistry`].
//! 2. For each slot:
//!    1. Skip unless `activity == Spawning`. A slot with `shell_pid > 0`
//!       (a real process reported in) belongs to `mark_stalled_spawns`
//!       or `dead_pid_sweep`, not here.
//!    2. Skip if `shell_pid > 0` — some process did report in; this
//!       sweep only owns the total-silence case.
//!    3. Skip if any hook has ever fired (`last_event_at.is_some()`) —
//!       proof of life even without a pid.
//!    4. Age guard against the DB `started_at` ([`SPAWN_ACK_GRACE_SECS`]):
//!       skip executions dispatched too recently to have exhausted the
//!       app's own async surface-creation + shell-pid-retry window.
//! 3. For a confirmed spawn-ack timeout: mark the execution `orphaned`,
//!    append an `[engine-reconcile]` audit line, reap the (possibly
//!    ghost) app pane through the same `release_worker_pane` teardown
//!    `bossctl agents stop` uses, release the pool slot, emit a
//!    `spawn_ack_timeout` dispatch event, and kick the coordinator so
//!    the orphan sweep redispatches the never-started work.
//!
//! ## False-positive guards
//!
//! [`SPAWN_ACK_GRACE_SECS`] (60s) is deliberately well above the app's
//! shell-pid-propagation retry window (a single 250ms retry after
//! `onSurfaceAttached`) so a merely-slow-but-real spawn is never reaped.
//! Any slot that reports a pid or emits a single hook before the grace
//! elapses is left alone — this sweep only fires on total silence.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::stale_worker_sweep`]).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use boss_protocol::{WorkItemPatch, WorkerActivity};

use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::work::WorkDb;

/// Grace period after `started_at` (epoch seconds) during which a
/// pid-less, hook-less `Spawning` slot is left alone. Comfortably above
/// the app's shell-pid-report retry window (one 250ms retry) and above
/// [`crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS`] (30s) so
/// this sweep never races a spawn that is merely slow but genuinely
/// alive — by the time this threshold elapses with zero pid and zero
/// hook, nothing reported in at all.
pub const SPAWN_ACK_GRACE_SECS: i64 = 60;

/// Reaps a confirmed spawn-ack-timeout slot's (possibly ghost) app pane
/// and process tree, mirroring [`crate::stale_worker_sweep::StaleWorkerReaper`].
/// A pid-less spawn has nothing for a direct `kill(pid, 0)` to act on,
/// but the app may still be holding a `TerminalPaneSession` for the
/// slot (surface creation started but never produced a live shell) —
/// tearing it down through `release_worker_pane` is what lets the next
/// dispatch reuse the slot instead of the app rejecting the respawn
/// with `SlotBusy`.
#[async_trait::async_trait]
pub trait SpawnAckReaper: Send + Sync {
    /// Tear down the app pane (if any) and release resources for
    /// `execution_id`. Idempotent: a slot with no real pane at all is a
    /// no-op.
    async fn reap_worker(&self, execution_id: &str);
}

/// Counts from one pass of the sweep; logged at `info` when a reap
/// occurs.
#[derive(Debug, Default)]
pub struct SpawnAckSweepOutcome {
    pub reaped: usize,
    pub has_pid_skipped: usize,
    pub has_event_skipped: usize,
    pub not_spawning_skipped: usize,
    pub grace_skipped: usize,
}

impl crate::sweep_loop::SweepOutcome for SpawnAckSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            has_pid_skipped = self.has_pid_skipped,
            has_event_skipped = self.has_event_skipped,
            grace_skipped = self.grace_skipped,
            "spawn-ack sweep: pass complete",
        );
    }
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`.
/// Fires immediately on spawn so a false-live spawn stranded before the
/// engine restarted is recovered at boot without waiting for the first
/// interval.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    reaper: Arc<dyn SpawnAckReaper>,
    interval: Duration,
    grace_secs: i64,
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
                grace_secs,
            )
            .await
        }
    })
}

/// Run a single spawn-ack sweep pass. Returns a summary of what
/// happened; callers may log it.
///
/// Takes `coordinator` as `Arc` because kicking the scheduler requires
/// `Arc<ExecutionCoordinator>` — the kick path spawns a tokio task that
/// holds a reference.
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    reaper: &dyn SpawnAckReaper,
    grace_secs: i64,
) -> SpawnAckSweepOutcome {
    let mut outcome = SpawnAckSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let grace_cutoff = now_epoch_secs - grace_secs;

    for state in snapshot {
        // Only total-silence `Spawning` slots are candidates. Anything
        // else — `WaitingForInput`, `Working`, `Idle` — has already
        // shown some sign of life and belongs to a different sweep.
        if state.activity != WorkerActivity::Spawning {
            outcome.not_spawning_skipped += 1;
            continue;
        }

        // A reported pid means dead_pid_sweep (if it later dies) or the
        // directory-trust-prompt path (mark_stalled_spawns) owns this
        // slot — not us.
        if state.shell_pid > 0 {
            outcome.has_pid_skipped += 1;
            continue;
        }

        // Any hook at all is proof of life even without a pid.
        if state.last_event_at.is_some() {
            outcome.has_event_skipped += 1;
            continue;
        }

        let execution_id = &state.run_id;

        let execution = match work_db.get_execution(execution_id) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(
                    execution_id,
                    ?err,
                    "spawn-ack sweep: failed to look up execution; skipping slot",
                );
                continue;
            }
        };

        // Skip executions already in a terminal DB state (completion
        // path may have raced the sweep).
        if execution.status.is_terminal() {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within `grace_secs` or not yet recorded.
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

        tracing::info!(
            execution_id,
            work_item_id = %execution.work_item_id,
            slot_id = state.slot_id,
            "spawn-ack sweep: no shell pid and no hook event since spawn; reaping execution and releasing slot",
        );

        // Mark the execution orphaned so the DB reflects the false-live
        // spawn and `bossctl agents transcript <exec-id>` still works.
        let reason = format!(
            "spawn-ack-timeout: no shell pid reported and no hook event received within {grace_secs}s of spawn; worker process never came up"
        );
        if let Err(err) = work_db.mark_execution_orphaned(execution_id, &reason) {
            tracing::warn!(
                execution_id,
                ?err,
                "spawn-ack sweep: failed to mark execution orphaned; skipping reap",
            );
            continue;
        }

        // Snapshot any uncommitted workspace work to a durable patch
        // before the slot is released and the workspace becomes
        // eligible for re-lease/reset. Best-effort: a false-live spawn
        // typically has nothing to back up, but this is a no-op-safe
        // call mirroring the other sweeps.
        let recovery_patch = crate::recovery_backup::backup_dead_execution(&execution);

        // Append [engine-reconcile] audit line to the task description
        // so a human inspecting the chore can see why it was reset.
        if let Some(work_item_id) = &state.work_item_id
            && let Err(err) = append_reconcile_audit(
                work_db,
                work_item_id,
                execution_id,
                now_epoch_secs,
                grace_secs,
                recovery_patch.as_deref(),
            )
        {
            tracing::warn!(
                work_item_id,
                ?err,
                "spawn-ack sweep: failed to append audit line to description (non-fatal)",
            );
        }

        // Tear down the (possibly ghost) app pane BEFORE the pool slot
        // is released, mirroring the stale-worker sweep's ordering —
        // otherwise a redispatch to the same slot could hit `SlotBusy`
        // if the app is still holding a `TerminalPaneSession` whose
        // surface never produced a live shell.
        reaper.reap_worker(execution_id).await;

        // Release the worker pool slot so the orphan sweep detects the
        // chore and creates a fresh ready execution for redispatch.
        // Idempotent with the pool-slot release production's
        // `release_worker_pane` already performs (find-or-skip no-op);
        // in tests where the reaper is a recording stub, this is what
        // frees the slot.
        let worker_id = worker_id_for_slot(state.slot_id);
        coordinator.release_worker_and_kick(&worker_id, None).await;

        // Structured event for bossctl dispatch tail.
        dispatch_events
            .emit(
                DispatchEvent::new(Stage::SpawnAckTimeout, Outcome::Ok, execution_id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "slot_id": state.slot_id,
                        "shell_pid": state.shell_pid,
                        "threshold_secs": grace_secs,
                        "recovery_patch": recovery_patch
                            .as_deref()
                            .map(|p| p.display().to_string()),
                    })),
            )
            .await;

        outcome.reaped += 1;
    }

    outcome
}

/// Append an `[engine-reconcile]` audit line to the work item's
/// description so an operator can see why the chore was reset.
fn append_reconcile_audit(
    work_db: &WorkDb,
    work_item_id: &str,
    dead_execution_id: &str,
    now_epoch_secs: i64,
    grace_secs: i64,
    recovery_patch: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let item = work_db.get_work_item(work_item_id)?;
    let current_desc = match &item {
        boss_protocol::WorkItem::Product(p) => p.description.as_str(),
        boss_protocol::WorkItem::Project(p) => p.description.as_str(),
        boss_protocol::WorkItem::Task(t) | boss_protocol::WorkItem::Chore(t) => t.description.as_str(),
    };
    let recovery_note = match recovery_patch {
        Some(path) => format!(" Uncommitted work backed up to {}.", path.display()),
        None => String::new(),
    };
    let audit_line = format!(
        "\n[engine-reconcile] epoch {now_epoch_secs}: spawn-ack timeout (exec {dead_execution_id}) detected — no shell pid or hook event within {grace_secs}s of spawn; chore reset to todo for redispatch.{recovery_note}"
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
    use boss_protocol::{WorkItemBinding, WorkerEvent};
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

    // ─── stubs (mirrors dead_pid_sweep / stale_worker_sweep) ─────────────────

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

    /// Records every `reap_worker` call and, at reap time, snapshots
    /// whether the execution's pool slot is still claimed — proves the
    /// reap ran BEFORE the slot/lease was released, mirroring
    /// `stale_worker_sweep`'s ordering test.
    struct RecordingReaper {
        coordinator: Arc<ExecutionCoordinator>,
        reaped: StdMutex<Vec<(String, bool)>>,
    }

    impl RecordingReaper {
        fn new(coordinator: Arc<ExecutionCoordinator>) -> Self {
            Self {
                coordinator,
                reaped: StdMutex::new(Vec::new()),
            }
        }

        fn reaped(&self) -> Vec<(String, bool)> {
            self.reaped.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl SpawnAckReaper for RecordingReaper {
        async fn reap_worker(&self, execution_id: &str) {
            let still_claimed = self
                .coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(execution_id);
            self.reaped
                .lock()
                .unwrap()
                .push((execution_id.to_owned(), still_claimed));
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

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
    /// `started_at` to 5 minutes ago so the grace-period guard passes.
    fn create_old_execution(db: &WorkDb, work_item_id: &str) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(RequestExecutionInput::builder().work_item_id(work_item_id).build())
            .unwrap();
        let old_started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(300) as i64; // 5 minutes ago
        db.force_started_at_for_test(&execution.id, old_started_at).unwrap();
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

    fn register_slot_zero_pid(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        work_item_id: &str,
    ) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            0,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
    }

    // ─── tests ───────────────────────────────────────────────────────────────

    /// The core invariant: a `Spawning` slot with `shell_pid == 0` and no
    /// hook events, past the grace window, has its execution orphaned,
    /// its pane reaped, its pool slot released, and a `spawn_ack_timeout`
    /// dispatch event emitted.
    #[tokio::test]
    async fn silent_zero_pid_spawn_is_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id)
        );

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 1, "silent zero-pid spawn must be reaped");

        let exec = db.get_execution(&execution_id).unwrap();
        assert_eq!(exec.status, ExecutionStatus::Orphaned);

        let claimed_after = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(!claimed_after.contains(&execution_id), "pool slot must be released");

        // Reap ran before the slot/lease was released.
        assert_eq!(reaper.reaped(), vec![(execution_id.clone(), true)]);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "spawn_ack_timeout");
        assert_eq!(events[0].outcome, "ok");
        assert_eq!(events[0].work_item_id.as_deref(), Some(work_item_id.as_str()));

        let item = db.get_work_item(&work_item_id).unwrap();
        let desc = match &item {
            boss_protocol::WorkItem::Chore(t) | boss_protocol::WorkItem::Task(t) => t.description.clone(),
            _ => panic!("expected chore"),
        };
        assert!(desc.contains("[engine-reconcile]"), "got: {desc:?}");
    }

    /// A slot that reported a real shell pid is never reaped by this
    /// sweep, even if it never emitted a hook — that's `mark_stalled_spawns`
    /// (or `dead_pid_sweep` if the pid later dies) territory.
    #[tokio::test]
    async fn slot_with_reported_pid_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        live_states.register_spawn(
            1,
            &execution_id,
            "claude-opus-4-7",
            std::process::id() as i32,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.clone(),
            }),
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "a slot with a reported pid must not be reaped here");
        assert_eq!(outcome.has_pid_skipped, 1);
        assert!(sink.events().await.is_empty());
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    }

    /// A pid-less slot that has emitted at least one hook event is proof
    /// of life and must not be reaped.
    #[tokio::test]
    async fn slot_with_any_hook_event_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
        // SessionStart with a resume source is proof of life without
        // flipping activity away from Spawning (only the Startup source
        // does that) — this isolates the has_event guard from the
        // not_spawning guard exercised by the test below.
        live_states.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".to_owned(),
                source: boss_protocol::SessionStartSource::Resume,
            },
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "a slot with any hook event must not be reaped");
        assert!(sink.events().await.is_empty());
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    }

    /// A silent zero-pid slot whose execution started within the grace
    /// window is left alone — guards against racing a fresh dispatch
    /// whose app-side surface is still asynchronously coming up.
    #[tokio::test]
    async fn recent_started_at_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        db.force_started_at_for_test(&execution.id, now_secs).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution.id, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh dispatches");
        assert_eq!(outcome.grace_skipped, 1);
    }

    /// A slot already past `Spawning` (e.g. `Working`) is never a
    /// candidate for this sweep, regardless of pid/hook state.
    #[tokio::test]
    async fn non_spawning_activity_is_skipped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id);
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
        live_states.apply_event(
            1,
            &WorkerEvent::UserPromptSubmit {
                session_id: "s".to_owned(),
                prompt: "go".to_owned(),
            },
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.not_spawning_skipped, 1);
    }
}
