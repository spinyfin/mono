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
use std::time::Duration;

use boss_protocol::{WorkExecution, WorkerActivity};

use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::spawn_health::{SpawnHealthTracker, maybe_admit_recovery_probe, trip_spawn_capability_circuit};
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
#[allow(clippy::too_many_arguments)]
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    reaper: Arc<dyn SpawnAckReaper>,
    spawn_health: Arc<SpawnHealthTracker>,
    interval: Duration,
    grace_secs: i64,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let reaper = Arc::clone(&reaper);
        let spawn_health = Arc::clone(&spawn_health);
        async move {
            run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                reaper.as_ref(),
                spawn_health.as_ref(),
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
#[allow(clippy::too_many_arguments)]
pub async fn run_one_pass(
    work_db: &WorkDb,
    live_states: &LiveWorkerStateRegistry,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: &dyn DispatchEventSink,
    reaper: &dyn SpawnAckReaper,
    spawn_health: &SpawnHealthTracker,
    grace_secs: i64,
) -> SpawnAckSweepOutcome {
    let mut outcome = SpawnAckSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let grace_cutoff = now_epoch_secs - grace_secs;
    let ctx = SpawnReapCtx {
        work_db,
        coordinator: Arc::clone(&coordinator),
        dispatch_events,
        reaper,
        spawn_health,
    };

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

        let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
            work_db,
            execution_id,
            "spawn-ack sweep: failed to look up execution; skipping slot",
        ) else {
            continue;
        };

        // Skip executions already in a terminal DB state (completion
        // path may have raced the sweep).
        if execution.status.is_terminal() {
            continue;
        }

        // Grace-period guard: skip executions whose `started_at` is
        // within `grace_secs` or not yet recorded.
        let started_epoch = execution.started_epoch();
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

        if reap_never_started_spawn(
            &ctx,
            &execution,
            state.slot_id,
            state.shell_pid,
            ReapCause::SpawnAckTimeout { grace_secs },
            now_epoch_secs,
        )
        .await
        {
            outcome.reaped += 1;
        }
    }

    // Breaker half-open recovery: while dispatch is Breaker-paused, this is
    // the tick that periodically admits a single canary execution through
    // the pause. Runs every pass regardless of whether this pass reaped
    // anything — the breaker may have tripped from an app NACK (a different
    // code path) rather than from a timeout seen above.
    maybe_admit_recovery_probe(work_db, &coordinator, spawn_health, now_epoch_secs).await;

    outcome
}

/// Shared references the reap path needs, bundled so
/// [`reap_never_started_spawn`] stays under the argument-count lint and both
/// callers (the periodic sweep and the `ReportWorkerSpawnFailed` NACK handler)
/// construct it the same way.
pub(crate) struct SpawnReapCtx<'a> {
    pub work_db: &'a WorkDb,
    /// `Arc` because `release_worker_and_kick` spawns a task that holds a
    /// coordinator reference.
    pub coordinator: Arc<ExecutionCoordinator>,
    pub dispatch_events: &'a dyn DispatchEventSink,
    pub reaper: &'a dyn SpawnAckReaper,
    pub spawn_health: &'a SpawnHealthTracker,
}

/// Why a never-started spawn is being reaped. Selects the orphan reason text,
/// the `[engine-reconcile]` audit note, and the dispatch stage emitted.
pub(crate) enum ReapCause<'a> {
    /// The periodic sweep found total silence past the grace window.
    SpawnAckTimeout { grace_secs: i64 },
    /// The app proactively reported the spawn failed (fast-fail NACK).
    AppNack { reason: &'a str },
}

/// Reap a `Spawning` slot that never produced a live shell: mark the execution
/// orphaned, back up any uncommitted work, append an `[engine-reconcile]`
/// audit line, tear down the (possibly ghost) app pane, release the pool slot,
/// emit a dispatch event, and feed the spawn-capability circuit breaker —
/// tripping it when too many DISTINCT work items fail in the window. Returns
/// `true` when the execution was reaped, `false` when it was skipped (already
/// terminal, or the orphan write failed).
///
/// Shared by [`run_one_pass`] (the 60s timeout path) and
/// [`crate::app::sessions::handle_report_worker_spawn_failed`] (the immediate
/// NACK path) so both do exactly the same thing — the only difference is
/// `cause`.
pub(crate) async fn reap_never_started_spawn(
    ctx: &SpawnReapCtx<'_>,
    execution: &WorkExecution,
    slot_id: u8,
    shell_pid: i32,
    cause: ReapCause<'_>,
    now_epoch_secs: i64,
) -> bool {
    let execution_id = execution.id.as_str();
    let work_item_id = execution.work_item_id.as_str();

    let (orphan_reason, audit_note, stage) = match &cause {
        ReapCause::SpawnAckTimeout { grace_secs } => (
            format!(
                "spawn-ack-timeout: no shell pid reported and no hook event received within {grace_secs}s of spawn; worker process never came up"
            ),
            format!(
                "spawn-ack timeout (exec {execution_id}) detected — no shell pid or hook event within {grace_secs}s of spawn; chore reset to todo for redispatch."
            ),
            Stage::SpawnAckTimeout,
        ),
        ReapCause::AppNack { reason } => (
            format!("app reported spawn failure (no shell): {reason}"),
            format!(
                "app reported worker-pane spawn failure (exec {execution_id}): {reason}; chore reset to todo for redispatch."
            ),
            Stage::SpawnNack,
        ),
    };

    if let Err(err) = ctx.work_db.mark_execution_orphaned(execution_id, &orphan_reason) {
        tracing::warn!(
            execution_id,
            ?err,
            "reap-never-started-spawn: failed to mark execution orphaned; skipping reap",
        );
        return false;
    }

    // Snapshot any uncommitted workspace work to a durable patch before the
    // slot is released and the workspace becomes eligible for re-lease/reset.
    // Best-effort: a false-live spawn typically has nothing to back up.
    let recovery_patch = crate::recovery_backup::backup_dead_execution(execution);

    // Append an [engine-reconcile] audit line to the work item's description
    // so a human inspecting the chore can see why it was reset.
    if let Err(err) = crate::reconcile_audit::append_reconcile_audit(
        ctx.work_db,
        work_item_id,
        now_epoch_secs,
        &audit_note,
        recovery_patch.as_deref(),
    ) {
        tracing::warn!(
            work_item_id,
            ?err,
            "reap-never-started-spawn: failed to append audit line to description (non-fatal)",
        );
    }

    // Tear down the (possibly ghost) app pane BEFORE the pool slot is
    // released, mirroring the stale-worker sweep's ordering — otherwise a
    // redispatch to the same slot could hit `SlotBusy` if the app is still
    // holding a `TerminalPaneSession` whose surface never produced a shell.
    ctx.reaper.reap_worker(execution_id).await;

    // Release the worker pool slot so the orphan sweep detects the chore and
    // creates a fresh ready execution for redispatch. Idempotent with the
    // pool-slot release production's `release_worker_pane` already performs.
    let worker_id = worker_id_for_slot(slot_id);
    ctx.coordinator.release_worker_and_kick(&worker_id, None).await;

    // Structured event for bossctl dispatch tail.
    let mut details = serde_json::json!({
        "slot_id": slot_id,
        "shell_pid": shell_pid,
        "recovery_patch": recovery_patch.as_deref().map(|p| p.display().to_string()),
    });
    match &cause {
        ReapCause::SpawnAckTimeout { grace_secs } => {
            details["threshold_secs"] = serde_json::json!(grace_secs);
        }
        ReapCause::AppNack { reason } => {
            details["reason"] = serde_json::json!(reason);
        }
    }
    ctx.dispatch_events
        .emit(
            DispatchEvent::new(stage, Outcome::Ok, execution_id)
                .with_work_item(work_item_id)
                .with_details(details),
        )
        .await;

    // Feed the cross-work-item spawn-capability breaker. A systemic post-wake
    // failure spreads across many work items, which the per-item churn guard
    // cannot catch; when enough DISTINCT items fail in the window the breaker
    // pauses dispatch and raises one loud attention item.
    if let Some(distinct) = ctx.spawn_health.record_failure(work_item_id, now_epoch_secs) {
        trip_spawn_capability_circuit(
            ctx.work_db,
            ctx.coordinator.as_ref(),
            ctx.dispatch_events,
            ctx.spawn_health,
            execution_id,
            work_item_id,
            distinct,
            now_epoch_secs,
        )
        .await;
    }

    // If this reap was the in-flight half-open recovery probe (see
    // `maybe_admit_recovery_probe`), the canary failed — back off before the
    // next attempt. No-op for any other execution.
    ctx.spawn_health.record_probe_failure(execution_id, now_epoch_secs);

    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use boss_protocol::{WorkItemBinding, WorkerEvent};

    use super::*;
    use crate::coordinator::ExecutionCoordinator;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::LiveWorkerStateRegistry;
    use crate::test_support::*;
    use crate::work::ExecutionStatus;

    // ─── stubs (mirrors dead_pid_sweep / stale_worker_sweep) ─────────────────
    // `NoopCube` / `NoopRunner` come from `crate::test_support::*`.

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
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
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
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
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
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
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
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
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
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
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
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
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
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .build(),
            )
            .unwrap();
        let now_secs = boss_engine_utils::epoch_time::now_epoch_secs();
        db.force_started_at_for_test(&execution.id, now_secs).unwrap();

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution.id, &work_item_id);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution.id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
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
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
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
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.not_spawning_skipped, 1);
    }

    /// The post-wake systemic failure: several DIFFERENT work items each have
    /// a silent zero-pid spawn. Once the distinct-work-item threshold is
    /// crossed in one pass, the spawn-capability breaker trips — dispatch is
    /// paused and a single `spawn_capability_unhealthy` event fires — instead
    /// of each item independently churning into its own churn guard.
    #[tokio::test]
    async fn systemic_spawn_failure_trips_capability_breaker_once() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        // Four distinct chores, each with a silent zero-pid spawn in its slot.
        let mut execution_ids = Vec::new();
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        for slot in 1u8..=4 {
            let work_item_id = create_active_chore(&db, &product_id, &format!("chore {slot}"));
            let execution_id = create_old_execution(&db, &work_item_id);
            register_slot_zero_pid(&live_states, slot, &execution_id, &work_item_id);
            execution_ids.push(execution_id);
        }

        // `AlwaysSucceedsCube`/`AlwaysSucceedsRunner`, not the panic-on-any-call
        // `Noop*` doubles: once the breaker trips and pauses dispatch, the
        // reap's steady-state rescan (`rescan_active_dispatch_after_release`)
        // immediately re-queues each reaped active chore as `ready`, and the
        // half-open recovery probe (`maybe_admit_recovery_probe`, run at the
        // end of this same sweep pass) force-dispatches one of them as a
        // canary — a real dispatch attempt this coordinator must be able to
        // carry through.
        let coordinator = make_dispatchable_coordinator(db.clone(), 4);
        for execution_id in &execution_ids {
            coordinator.worker_pool().claim_worker(execution_id, None).await;
        }
        assert!(!coordinator.is_dispatch_paused(), "precondition: dispatch running");

        // Threshold of 3 distinct work items; the 4th slot exercises
        // idempotency (already paused → no second signal).
        let spawn_health = SpawnHealthTracker::with_config(3, 300);
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 4, "every silent spawn is reaped");
        assert!(
            coordinator.is_dispatch_paused(),
            "breaker must pause dispatch once the distinct-work-item threshold is crossed",
        );

        let events = sink.events().await;
        let unhealthy: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "spawn_capability_unhealthy")
            .collect();
        assert_eq!(
            unhealthy.len(),
            1,
            "exactly ONE loud signal despite 4 failures (idempotent while paused)",
        );
        assert_eq!(unhealthy[0].outcome, "error");
        assert_eq!(unhealthy[0].details["distinct_work_items"], serde_json::json!(3));

        // The one attention item is raised against the tripping execution.
        let tripping_exec = unhealthy[0].execution_id.clone();
        let attn = db.list_attention_items(&tripping_exec).unwrap();
        assert!(
            attn.iter()
                .any(|a| a.kind == crate::spawn_health::SPAWN_CAPABILITY_ATTENTION_KIND),
            "a loud app_spawn_capability_unhealthy attention item must be raised",
        );
    }

    /// Regression for the case where an *operator* pause is already active
    /// (which exempts `pr_review` executions from dispatch) when the app
    /// spawn path independently breaks. Before this fix, `record_failure`
    /// events feeding `trip_spawn_capability_circuit` would see
    /// `is_dispatch_paused() == true` and skip — never escalating the pause
    /// to `Breaker` origin, so reviews kept dispatching into a known-dead
    /// spawn path forever. The breaker must instead detect "paused but still
    /// review-exempt" and escalate: flip the origin to `Breaker` so reviews
    /// stop being exempt too.
    #[tokio::test]
    async fn breaker_escalates_operator_pause_to_clear_review_exemption() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let db = Arc::new(db);

        let mut execution_ids = Vec::new();
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        for slot in 1u8..=3 {
            let work_item_id = create_active_chore(&db, &product_id, &format!("chore {slot}"));
            let execution_id = create_old_execution(&db, &work_item_id);
            register_slot_zero_pid(&live_states, slot, &execution_id, &work_item_id);
            execution_ids.push(execution_id);
        }

        // See the comment in `systemic_spawn_failure_trips_capability_breaker_once`
        // for why this needs a coordinator that can actually carry a dispatch
        // through (the recovery probe force-dispatches a real ready row).
        let coordinator = make_dispatchable_coordinator(db.clone(), 3);
        for execution_id in &execution_ids {
            coordinator.worker_pool().claim_worker(execution_id, None).await;
        }

        // Operator pause is already active before the spawn path breaks —
        // this is what exempts pr_review executions from the pause.
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        coordinator.set_dispatch_paused(
            true,
            now.max(0) as u64,
            crate::coordinator::DispatchPauseOrigin::Operator,
        );
        assert!(
            coordinator.dispatch_pause_exempts_reviews(),
            "precondition: operator pause exempts reviews"
        );

        let spawn_health = SpawnHealthTracker::with_config(3, 300);
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            SPAWN_ACK_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 3, "every silent spawn is reaped");
        assert!(coordinator.is_dispatch_paused(), "dispatch remains paused");
        assert!(
            !coordinator.dispatch_pause_exempts_reviews(),
            "breaker trip must escalate an operator pause to Breaker origin, clearing the \
             review exemption so reviews stop dispatching into the dead spawn path",
        );

        let events = sink.events().await;
        let unhealthy: Vec<_> = events
            .iter()
            .filter(|e| e.stage == "spawn_capability_unhealthy")
            .collect();
        assert_eq!(
            unhealthy.len(),
            1,
            "the breaker trip must still raise its loud signal despite the pre-existing pause",
        );
    }

    /// The fast-fail NACK path: `reap_never_started_spawn` with the `AppNack`
    /// cause (what `handle_report_worker_spawn_failed` calls) reaps the
    /// execution immediately, orphans it, releases the slot, and emits a
    /// `spawn_nack` event carrying the app-supplied reason. A single NACK is
    /// below the distinct-work-item threshold, so the breaker does NOT trip.
    #[tokio::test]
    async fn app_nack_reaps_and_emits_spawn_nack_event() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let execution = db.get_execution(&execution_id).unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let spawn_health = SpawnHealthTracker::new();
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let ctx = SpawnReapCtx {
            work_db: db.as_ref(),
            coordinator: coordinator.clone(),
            dispatch_events: sink.as_ref(),
            reaper: reaper.as_ref(),
            spawn_health: &spawn_health,
        };
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let reason = "ghostty_surface_new returned NULL (no active display)";
        let reaped = reap_never_started_spawn(&ctx, &execution, 1, 0, ReapCause::AppNack { reason }, now).await;

        assert!(reaped, "app NACK must reap the never-started spawn");
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "pool slot must be released so the freed slot is reusable",
        );

        let events = sink.events().await;
        let nack: Vec<_> = events.iter().filter(|e| e.stage == "spawn_nack").collect();
        assert_eq!(nack.len(), 1, "AppNack cause must emit exactly one spawn_nack event");
        assert_eq!(nack[0].outcome, "ok");
        assert_eq!(nack[0].details["reason"], serde_json::json!(reason));
        // One NACK is below the distinct-work-item threshold — no breaker trip.
        assert!(
            !coordinator.is_dispatch_paused(),
            "a single NACK must not trip the breaker"
        );
        assert!(events.iter().all(|e| e.stage != "spawn_capability_unhealthy"));
    }
}
