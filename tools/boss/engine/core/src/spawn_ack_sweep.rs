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
//! ## The second incident: a pane that DID have a shell, and no driver
//!
//! On 2026-07-30 the inverse shape appeared. `pane_spawned/ok` came back,
//! `onSurfaceAttached` reported a real foreground pid (92697), and the
//! slot looked healthy to every check in Boss — but the pid was the
//! **login shell's**, and the driver binary had never been exec'd at all
//! (the spawn command was delivered as typed tty input and eaten by the
//! macOS canonical-mode line-length cap; that trigger is fixed
//! separately). `bossctl agents transcript` reported "engine has not yet
//! received a hook event carrying transcript_path" indefinitely. The
//! merge poller logged "still waiting_human with no pr_url; will retry"
//! every ~68s forever. No attention item was ever raised. The slot and
//! its cube workspace lease were held until a human noticed the pane.
//!
//! Every gate passed *because* a pid existed:
//!
//! - this sweep skipped the slot outright (`shell_pid > 0`);
//! - [`LiveWorkerStateRegistry::mark_stalled_spawns`] skipped it too,
//!   because grok omits `Capability::AwaitingInputSignal`;
//! - [`crate::dead_pid_sweep`]'s `kill(pid, 0)` found the login shell
//!   very much alive.
//!
//! The common root: **nothing validated the driver process.** Boss
//! validated the pane surface and the shell hosting it, and treated that
//! as proof of a working worker. So this module now reaps on two
//! independent causes.
//!
//! ## Algorithm
//!
//! ### Pass 1 — spawn-ack timeout (the 2026-07-03/04 class)
//!
//! Snapshot [`LiveWorkerStateRegistry`]; for each slot:
//!
//! 1. Skip unless `activity == Spawning`.
//! 2. Skip if a driver-originated signal was ever recorded
//!    ([`LiveWorkerStateRegistry::driver_signal_at`]) — the driver is
//!    running, whatever else is wrong.
//! 3. Skip if `shell_pid > 0`. **This is a scope split, not a health
//!    verdict** — a slot with a pid is pass 2's, on a longer window,
//!    because the pid means the app really did host something and the
//!    question narrows to "is that something the driver?".
//! 4. Age guard against the DB `started_at` ([`SPAWN_ACK_GRACE_SECS`]).
//!
//! ### Pass 2 — driver-start timeout (the 2026-07-30 class)
//!
//! [`LiveWorkerStateRegistry::unverified_driver_starts`] returns every
//! live slot past [`crate::live_worker_state::DRIVER_START_GRACE_SECS`] with no driver-originated
//! signal. That query reads only `driver_signal_at` and `spawned_at`, so
//! it is blind to `shell_pid`, to `activity`, and to the driver's
//! capability set: it covers grok exactly as it covers claude, and it
//! sees a slot `mark_stalled_spawns` has promoted out of `Spawning` just
//! as well as one still sitting in it.
//!
//! Both passes funnel into [`reap_never_started_spawn`], which marks the
//! execution `orphaned`, appends an `[engine-reconcile]` audit line,
//! reaps the pane through the same `release_worker_pane` teardown
//! `bossctl agents stop` uses, releases the pool slot, force-releases the
//! cube workspace lease, emits a dispatch event, and kicks the
//! coordinator so the orphan sweep redispatches the never-started work.
//! Pass 2 additionally raises an attention item (see below).
//!
//! ## False-positive guards
//!
//! [`SPAWN_ACK_GRACE_SECS`] (60s) is deliberately well above the app's
//! shell-pid-propagation retry window (a single 250ms retry after
//! `onSurfaceAttached`) so a merely-slow-but-real spawn is never reaped.
//!
//! [`crate::live_worker_state::DRIVER_START_GRACE_SECS`] (300s) is five times that, and an order of
//! magnitude above real driver startup — a healthy driver's `SessionStart`
//! hook fires within seconds of exec. See that constant's doc for why
//! claude's folder-trust dialog, the one historically legitimate
//! multi-minute pre-hook wait, cannot produce a false positive here.
//!
//! A slot that produces a single driver signal before its window elapses
//! is left alone by both passes, permanently: `driver_signal_at` is
//! first-write-wins and is never cleared for the life of the run.
//!
//! ## Why pass 2 raises an attention item and pass 1 does not
//!
//! Both passes feed [`crate::spawn_health`] — the reap is shared, so every
//! cause records evidence, records a failure against the work item, and can
//! trip the spawn-capability breaker that pauses dispatch once enough
//! DISTINCT work items fail inside the window. That is deliberate for a
//! driver-start timeout too: a driver binary that cannot exec on this host
//! fails identically for every work item routed to it, which is exactly the
//! systemic shape the breaker exists to stop, and the alternative — reaping
//! and redispatching forever without ever pausing — is the churn the breaker
//! was built to end.
//!
//! What differs is *visibility*. Pass 1's failure is "the app's spawn path
//! is misbehaving", and the breaker's one loud attention item on trip is a
//! faithful summary of it. Pass 2's failure is different in kind: a pane
//! genuinely came up and a live process was left holding a workspace with
//! no driver in it. A single aggregate item cannot name which workspace is
//! still held, and a lone occurrence — the 2026-07-30 incident was one — is
//! below any aggregate threshold and would surface nowhere at all. So pass 2
//! additionally raises its own per-execution item
//! ([`DRIVER_START_ATTENTION_KIND`]) on top of the aggregation, rather than
//! instead of it.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::stale_worker_sweep`]).

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{CreateAttentionItemInput, LiveWorkerState, WorkExecution, WorkerActivity};

use crate::coordinator::{CubeClient, ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::spawn_health::{SpawnHealthTracker, maybe_admit_recovery_probe, trip_spawn_capability_circuit};
use crate::work::WorkDb;

/// Whether a live slot has shown **no proof of life whatsoever** — no shell
/// pid was ever reported, no hook event ever arrived, and it is still
/// advertising `Spawning`. Such a slot has an execution the engine believes
/// is running with, in fact, no process behind it: nothing was ever started,
/// so nothing can have died.
///
/// This is the classifier that decides which reap an app report earns. Both
/// app-originated reports about a not-yet-live pane land on it:
///
/// - `ReportWorkerSpawnFailed` (the diagnostic NACK) uses it as a staleness
///   guard — a slot that HAS shown proof of life must never be reaped by a
///   late NACK, because the pane demonstrably came up.
/// - `WorkerPaneDied` uses it to tell "the pane never came up" apart from
///   "the pane died after running". Only the latter is a death; the former
///   belongs on [`reap_never_started_spawn`], which feeds the cross-work-item
///   [`crate::spawn_health`] breaker. The pane-death path does not feed it,
///   and routing never-started spawns there is what let the 2026-07
///   no-active-display incident churn 818 executions across 79 work items
///   without the one aggregator that would have stopped it ever seeing a
///   single failure.
///
/// Pure and free-standing so the classification is unit-testable without a
/// live registry, and so both call sites are provably asking the same
/// question rather than maintaining two copies of the predicate.
pub(crate) fn slot_never_started(state: &LiveWorkerState) -> bool {
    state.shell_pid <= 0 && state.last_event_at.is_none() && state.activity == WorkerActivity::Spawning
}

/// Kind string for the attention item raised when a spawn produced a
/// pane but no driver. Stable — operator tooling pins it.
pub const DRIVER_START_ATTENTION_KIND: &str = "worker_driver_never_started";

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
    /// Reaped by pass 1 — nothing reported in at all.
    pub reaped: usize,
    /// Reaped by pass 2 — a pane came up but no driver ever signalled.
    pub driver_start_reaped: usize,
    /// Why pass 1 passed over the slots it did not reap.
    pub skipped: SpawnAckSkipCounts,
}

/// Pass 1's per-reason skip tallies, grouped so the outcome distinguishes
/// what the sweep *did* from why it declined — and so adding a reason
/// doesn't widen the outcome struct.
#[derive(Debug, Default)]
pub struct SpawnAckSkipCounts {
    /// Slot reported a pid, so it belongs to pass 2's longer window.
    pub has_pid: usize,
    /// A driver-originated signal was recorded: a hook or transcript path
    /// proving the driver runs, NOT merely "some event timestamp exists".
    pub has_driver_signal: usize,
    /// Slot has already left `Spawning`.
    pub not_spawning: usize,
    /// Execution is still inside [`SPAWN_ACK_GRACE_SECS`].
    pub grace: usize,
}

impl crate::sweep_loop::SweepOutcome for SpawnAckSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0 || self.driver_start_reaped > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            driver_start_reaped = self.driver_start_reaped,
            has_pid_skipped = self.skipped.has_pid,
            has_driver_signal_skipped = self.skipped.has_driver_signal,
            grace_skipped = self.skipped.grace,
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
    cube_client: Arc<dyn CubeClient>,
    interval: Duration,
    grace_secs: i64,
    driver_start_grace_secs: i64,
) -> tokio::task::JoinHandle<()> {
    crate::sweep_loop::spawn_sweep_loop(interval, move || {
        let work_db = Arc::clone(&work_db);
        let live_states = Arc::clone(&live_states);
        let coordinator = Arc::clone(&coordinator);
        let dispatch_events = Arc::clone(&dispatch_events);
        let reaper = Arc::clone(&reaper);
        let spawn_health = Arc::clone(&spawn_health);
        let cube_client = Arc::clone(&cube_client);
        async move {
            run_one_pass(
                work_db.as_ref(),
                live_states.as_ref(),
                coordinator.clone(),
                dispatch_events.as_ref(),
                reaper.as_ref(),
                spawn_health.as_ref(),
                cube_client.as_ref(),
                grace_secs,
                driver_start_grace_secs,
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
    cube_client: &dyn CubeClient,
    grace_secs: i64,
    driver_start_grace_secs: i64,
) -> SpawnAckSweepOutcome {
    let mut outcome = SpawnAckSweepOutcome::default();
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let grace_cutoff = now_epoch_secs - grace_secs;
    let ctx = SpawnReapCtx::builder()
        .work_db(work_db)
        .coordinator(Arc::clone(&coordinator))
        .dispatch_events(dispatch_events)
        .reaper(reaper)
        .spawn_health(spawn_health)
        .cube_client(cube_client)
        .build();

    for state in snapshot {
        // Only total-silence `Spawning` slots are candidates. Anything
        // else — `WaitingForInput`, `Working`, `Idle` — has already
        // shown some sign of life and belongs to a different sweep.
        //
        // NOTE this filter is NOT what covers the driver-never-started
        // class: `mark_stalled_spawns` can promote such a slot out of
        // `Spawning`, and it would escape here. Pass 2 below deliberately
        // ignores `activity` for exactly that reason.
        if state.activity != WorkerActivity::Spawning {
            outcome.skipped.not_spawning += 1;
            continue;
        }

        // A driver-originated signal — a hook, or a transcript path —
        // is the ONLY thing that proves the driver binary is running.
        // Checked before the pid split below so that a slot whose driver
        // is demonstrably alive is never a candidate for either pass.
        //
        // Replaces the old `last_event_at.is_some()` test, which was
        // forgeable: `mark_stalled_spawns` and `mark_errored` both write
        // that timestamp from engine-side inference, so the engine's own
        // guess could vouch for a driver that never ran.
        if live_states.driver_signal_at(state.slot_id).is_some() {
            outcome.skipped.has_driver_signal += 1;
            continue;
        }

        // A reported pid means the app really did host something for
        // this slot. That narrows the question from "did anything come
        // up?" to "is what came up the driver?" — a different question
        // on a longer window, owned by pass 2 below.
        //
        // This is a scope split between the two passes, NOT a health
        // verdict: before pass 2 existed, this `continue` was the end of
        // the line, and a pane hosting an idle login shell rode it to an
        // indefinite hold on a slot and a cube lease.
        if state.shell_pid > 0 {
            outcome.skipped.has_pid += 1;
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
                outcome.skipped.grace += 1;
                continue;
            }
            Some(t) if t >= grace_cutoff => {
                outcome.skipped.grace += 1;
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

    // ─── Pass 2: driver-start verification ──────────────────────────────
    //
    // Everything above answers "did a pane come up?". This answers the
    // question no check in Boss asked before 2026-07-30: "did the DRIVER
    // come up?" — the one a pane hosting an idle login shell fails while
    // satisfying every pane-level check indefinitely.
    //
    // `unverified_driver_starts` reads only `driver_signal_at` and
    // `spawned_at`. It is blind to `shell_pid`, to `activity`, and to the
    // driver's capability set by construction, so there is no driver-
    // specific path through it and no way to opt a driver out.
    for candidate in live_states.unverified_driver_starts(now_epoch_secs, driver_start_grace_secs) {
        let execution_id = &candidate.run_id;

        let Some(execution) = crate::sweep_loop::lookup_execution_or_warn(
            work_db,
            execution_id,
            "driver-start check: failed to look up execution; skipping slot",
        ) else {
            continue;
        };

        // The completion path may have raced us to a terminal status.
        if execution.status.is_terminal() {
            continue;
        }

        tracing::error!(
            execution_id,
            work_item_id = %execution.work_item_id,
            slot_id = candidate.slot_id,
            shell_pid = candidate.shell_pid,
            activity = candidate.activity.as_str(),
            silent_secs = candidate.silent_secs,
            threshold_secs = driver_start_grace_secs,
            "driver-start timeout: pane spawned but NO driver-originated signal (no hook event, no \
             transcript path) ever arrived. The reported shell pid is the login shell hosting the \
             pane, not the driver. Reaping: releasing the worker slot and the cube workspace lease \
             and raising an attention item.",
        );

        if reap_never_started_spawn(
            &ctx,
            &execution,
            candidate.slot_id,
            candidate.shell_pid,
            ReapCause::DriverStartTimeout {
                grace_secs: driver_start_grace_secs,
                silent_secs: candidate.silent_secs,
                activity: candidate.activity.as_str(),
            },
            now_epoch_secs,
        )
        .await
        {
            outcome.driver_start_reaped += 1;
        }
    }

    // Breaker half-open recovery: while dispatch is Breaker-paused, this is
    // the tick that periodically admits a single canary execution through
    // the pause. Runs every pass regardless of whether this pass reaped
    // anything — the breaker may have tripped from an app NACK (a different
    // code path) rather than from a timeout seen above.
    maybe_admit_recovery_probe(work_db, &coordinator, spawn_health, dispatch_events, now_epoch_secs).await;

    outcome
}

/// Shared references the reap path needs, bundled so
/// [`reap_never_started_spawn`] stays under the argument-count lint and both
/// callers (the periodic sweep and the `ReportWorkerSpawnFailed` NACK handler)
/// construct it the same way.
#[derive(bon::Builder)]
pub(crate) struct SpawnReapCtx<'a> {
    pub work_db: &'a WorkDb,
    /// `Arc` because `release_worker_and_kick` spawns a task that holds a
    /// coordinator reference.
    pub coordinator: Arc<ExecutionCoordinator>,
    pub dispatch_events: &'a dyn DispatchEventSink,
    pub reaper: &'a dyn SpawnAckReaper,
    pub spawn_health: &'a SpawnHealthTracker,
    /// Used to force-release the reaped execution's cube workspace lease.
    /// `mark_execution_orphaned` deliberately leaves the lease columns
    /// intact, and the pane teardown does not touch cube — so without
    /// this the workspace stays leased until TTL. Holding it silently is
    /// the harm the 2026-07-30 incident consisted of.
    pub cube_client: &'a dyn CubeClient,
}

/// Why a never-started spawn is being reaped. Selects the orphan reason text,
/// the `[engine-reconcile]` audit note, and the dispatch stage emitted.
pub(crate) enum ReapCause<'a> {
    /// The periodic sweep found total silence past the grace window.
    SpawnAckTimeout { grace_secs: i64 },
    /// The app proactively reported the spawn failed (fast-fail NACK).
    AppNack { reason: &'a str },
    /// The app reported the worker pane died (`WorkerPaneDied`), but the
    /// slot had never shown any proof of life — see [`slot_never_started`].
    /// The pane never came up, so this is a never-started spawn wearing a
    /// death report's clothing, and it is reaped as one.
    PaneDiedBeforeStart { detail: &'a str },
    /// A pane came up — possibly with a live shell pid — but no
    /// driver-originated signal ever arrived, so the driver binary never
    /// executed. Unlike the two above, this one also raises a
    /// per-execution attention item: nothing else in Boss surfaces it.
    DriverStartTimeout {
        grace_secs: i64,
        silent_secs: i64,
        activity: &'static str,
    },
}

/// The operator-facing narrative for one never-started reap: the orphan
/// reason recorded on the run, the `[engine-reconcile]` audit line appended
/// to the work item's description, and the dispatch stage emitted.
///
/// Pure and free-standing so each cause's story is unit-testable without a
/// DB, a pool, or an app session. That matters most for the reason text: it
/// is the only durable explanation an operator gets for why an execution
/// vanished, and a cause whose reason describes the wrong event (a "death"
/// for a pane that never came up) is indistinguishable from no explanation
/// at all.
fn reap_narrative(cause: &ReapCause<'_>, execution_id: &str) -> (String, String, Stage) {
    match &cause {
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
        ReapCause::PaneDiedBeforeStart { detail } => (
            format!(
                "pane-death-before-start: the app reported that {detail}, but no shell pid and no hook \
                 event was ever observed, so no worker process ever existed"
            ),
            format!(
                "app reported worker-pane death before start (exec {execution_id}): {detail}; no shell \
                 pid or hook event was ever observed, so the pane never came up; chore reset to todo \
                 for redispatch."
            ),
            Stage::PaneDeathBeforeStart,
        ),
        ReapCause::DriverStartTimeout {
            grace_secs,
            silent_secs,
            ..
        } => (
            format!(
                "driver-start-timeout: pane spawned but no driver-originated signal (hook event or \
                 transcript path) arrived within {grace_secs}s; driver binary never started \
                 (silent for {silent_secs}s)"
            ),
            format!(
                "driver-start timeout (exec {execution_id}) detected — pane came up but no hook event \
                 or transcript path arrived within {grace_secs}s, so the driver binary never ran; \
                 worker slot and cube workspace lease released, chore reset to todo for redispatch."
            ),
            Stage::DriverStartTimeout,
        ),
    }
}

/// Reap a `Spawning` slot that never produced a live shell: mark the execution
/// orphaned, back up any uncommitted work, append an `[engine-reconcile]`
/// audit line, tear down the (possibly ghost) app pane, release the pool slot,
/// emit a dispatch event, and feed the spawn-capability circuit breaker —
/// tripping it when too many DISTINCT work items fail in the window. Returns
/// `true` when the execution was reaped, `false` when it was skipped (already
/// terminal, or the orphan write failed).
///
/// Shared by [`run_one_pass`] (the 60s timeout path),
/// [`crate::app::sessions::handle_report_worker_spawn_failed`] (the immediate
/// NACK path), and [`crate::app::sessions::handle_worker_pane_died`] when the
/// reported "death" turns out to be a pane that never came up at all
/// ([`slot_never_started`]) — so all three do exactly the same thing, and in
/// particular all three feed the breaker. The only difference is `cause`.
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

    let (orphan_reason, audit_note, stage) = reap_narrative(&cause, execution_id);

    if let Err(err) = ctx.work_db.mark_execution_orphaned(execution_id, &orphan_reason) {
        tracing::warn!(
            execution_id,
            ?err,
            "reap-never-started-spawn: failed to mark execution orphaned; skipping reap",
        );
        return false;
    }

    // Never-started-spawn termination path: tear down any driver-owned
    // state outside the workspace. `mark_execution_orphaned` preserves
    // `workspace_path`, so the pre-call `execution` snapshot is still
    // current. Best-effort: a never-started spawn typically means
    // `provision_workspace` ran but `teardown_workspace` still gets its
    // chance regardless.
    crate::driver_teardown::teardown_driver_workspace(
        ctx.work_db,
        execution_id,
        execution.workspace_path.as_deref().map(std::path::Path::new),
        crate::driver_teardown::TeardownReason::SpawnAckTimeout,
    )
    .await;

    // Snapshot any uncommitted workspace work to a durable patch before the
    // slot is released and the workspace becomes eligible for re-lease/reset.
    // Best-effort: a false-live spawn typically has nothing to back up.
    let recovery_patch = boss_engine_recovery::recovery_backup::backup_dead_execution(execution);

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

    // Release the cube workspace lease. `mark_execution_orphaned`
    // deliberately leaves the lease columns intact (a live workspace may
    // hold in-flight commits a resume should reclaim) and nothing above
    // this line talks to cube — so before this call the lease survived
    // every reap on this path and stayed `leased` until TTL, kept warm by
    // the engine's own DB-fallback heartbeat.
    //
    // Here we KNOW no worker occupies the workspace: no driver ever
    // signalled, and the pane (with whatever shell it hosted) was torn
    // down above. Holding the lease "to be safe" is the harm, not the
    // safe option — that is what the 2026-07-30 incident was. Mirrors
    // `lost_workspace_sweep::run_one_pass`. Best-effort: a lease already
    // gone is the common benign case, so failure is `debug`, not `warn`.
    if let Some(lease_id) = execution.cube_lease_id.as_deref()
        && let Err(err) = ctx
            .cube_client
            .force_release_lease(lease_id, Some(orphan_reason.as_str()))
            .await
    {
        tracing::debug!(
            execution_id,
            lease_id,
            error = %format!("{err:#}"),
            "reap-never-started-spawn: best-effort cube lease force-release failed (likely already released)",
        );
    }

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
        ReapCause::PaneDiedBeforeStart { detail } => {
            details["detail"] = serde_json::json!(detail);
        }
        ReapCause::DriverStartTimeout {
            grace_secs,
            silent_secs,
            activity,
        } => {
            details["threshold_secs"] = serde_json::json!(grace_secs);
            details["silent_secs"] = serde_json::json!(silent_secs);
            details["activity"] = serde_json::json!(activity);
            raise_driver_start_attention(ctx.work_db, execution, slot_id, shell_pid, *grace_secs, *silent_secs);
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
    //
    // All four causes feed it, driver-start timeouts and app-reported pane
    // deaths before start included: a driver
    // binary that cannot exec on this host fails the same way for every work
    // item routed to it, so it belongs in the aggregate. Pass 2's own
    // per-execution attention item above is additional to this, not a
    // replacement for it — see the module doc.
    ctx.spawn_health
        .record_evidence(crate::spawn_health::SpawnFailureEvidence {
            execution_id: execution_id.to_owned(),
            work_item_id: work_item_id.to_owned(),
            slot_id: slot_id.to_string(),
            shell_pid,
            epoch_secs: now_epoch_secs,
        });
    if let Some(distinct) = ctx.spawn_health.record_failure(work_item_id, now_epoch_secs) {
        trip_spawn_capability_circuit(
            ctx.work_db,
            ctx.coordinator.as_ref(),
            ctx.dispatch_events,
            ctx.spawn_health,
            crate::spawn_health::TripSignal {
                tripping_execution_id: execution_id,
                tripping_work_item_id: work_item_id,
                distinct_work_items: distinct,
                now_epoch_secs,
            },
        )
        .await;
    }

    // If this reap was the in-flight half-open recovery probe (see
    // `maybe_admit_recovery_probe`), the canary failed — back off before the
    // next attempt. No-op for any other execution.
    ctx.spawn_health.record_probe_failure(execution_id, now_epoch_secs);

    true
}

/// Raise the per-execution attention item for a driver-start timeout.
///
/// The 2026-07-30 incident's defining property was silence: the merge
/// poller logged "still waiting_human with no pr_url; will retry" every
/// ~68 seconds indefinitely, and `attention_created` was `false`, so the
/// only thing that ever surfaced the stuck worker was a human happening
/// to look at the pane. This is the fix for that half — the reap frees
/// the resources, this makes the reap visible.
///
/// Deliberately per-execution rather than aggregated: unlike a spawn-ack
/// timeout (which redispatches transparently and is aggregated by
/// [`crate::spawn_health`]), a driver that never started with a live shell
/// left behind is a distinct condition an operator should see even when it
/// happens once.
///
/// Best-effort — a failure here must never abort the reap, since the reap
/// is what actually frees the slot and lease.
fn raise_driver_start_attention(
    work_db: &WorkDb,
    execution: &WorkExecution,
    slot_id: u8,
    shell_pid: i32,
    grace_secs: i64,
    silent_secs: i64,
) {
    let execution_id = execution.id.as_str();
    let pid_note = if shell_pid > 0 {
        format!(
            "The pane reported shell pid `{shell_pid}`, which is why every existing check treated \
             this slot as healthy — that pid is the **login shell hosting the pane**, not the driver."
        )
    } else {
        "No shell pid was ever reported for this pane.".to_owned()
    };
    let body = format!(
        "A worker pane was spawned for execution `{execution_id}` on slot {slot_id}, but no \
         driver-originated signal — no hook event, no `transcript_path` — arrived within \
         {grace_secs}s (silent for {silent_secs}s). The driver binary never started.\n\n\
         {pid_note}\n\n\
         The engine has reaped the execution: the pane was torn down, the worker slot released, \
         and the cube workspace lease force-released. The work item is reset for redispatch.\n\n\
         If this repeats for the same driver, the spawn command is most likely not reaching the \
         driver binary at all — check how the command is delivered to the pane rather than \
         whether the pane exists."
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        body_markdown: body,
        kind: DRIVER_START_ATTENTION_KIND.to_owned(),
        title: format!("Worker driver never started on slot {slot_id}"),
        execution_id: Some(execution_id.to_owned()),
        resolved_at: None,
        status: None,
        // Execution-scoped, not work-item-scoped: `create_attention_item`
        // rejects an input carrying both, and the execution is the right
        // anchor here — the failure is about this spawn, and the work item
        // is about to be redispatched onto a fresh one.
        work_item_id: None,
    }) {
        tracing::warn!(
            execution_id,
            ?err,
            "driver-start timeout: failed to raise attention item (reap still proceeded)",
        );
    }
}

/// End-to-end reproduction of the incident against a real OS process,
/// asserting that all three pre-existing guards pass the slot and only
/// driver-start verification catches it. Kept in its own file because it
/// drives several subsystems, not just this module.
#[cfg(test)]
#[path = "spawn_ack_sweep_induced_failure_tests.rs"]
mod induced_failure_tests;

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    use async_trait::async_trait;
    use boss_protocol::{WorkItemBinding, WorkerEvent};

    use super::*;
    use crate::coordinator::ExecutionCoordinator;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::live_worker_state::{DRIVER_START_GRACE_SECS, LiveWorkerStateRegistry};
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

    crate::stub_cube_client! { RecordingCube {
        async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> anyhow::Result<()> {
            self.released.lock().unwrap().push((lease_id.to_owned(), reason.map(str::to_owned)));
            Ok(())
        }
    } }

    /// Records every cube lease force-release so a reap can be asserted to
    /// have actually handed the workspace back, not merely orphaned the row.
    #[derive(Default)]
    struct RecordingCube {
        released: StdMutex<Vec<(String, Option<String>)>>,
    }

    impl RecordingCube {
        fn released_lease_ids(&self) -> Vec<String> {
            self.released.lock().unwrap().iter().map(|(id, _)| id.clone()).collect()
        }
    }

    // ─── helpers ─────────────────────────────────────────────────────────────

    /// Register a slot in the exact shape the 2026-07-30 incident left
    /// behind: a live foreground shell pid reported by the app, and no
    /// driver-originated signal at all.
    ///
    /// `awaiting_input_capable` mirrors the driver's declared capability —
    /// `false` is grok (which omits `Capability::AwaitingInputSignal` and was
    /// therefore exempt from `mark_stalled_spawns`), `true` is claude.
    fn register_slot_with_live_shell(
        live_states: &LiveWorkerStateRegistry,
        slot_id: u8,
        execution_id: &str,
        work_item_id: &str,
        shell_pid: i32,
        awaiting_input_capable: bool,
    ) {
        live_states.register_spawn_with_capabilities(
            slot_id,
            execution_id,
            "grok-4.5",
            shell_pid,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
            awaiting_input_capable,
            crate::live_worker_state::LiveSpawnRouting::none(),
        );
        // Age the spawn past every window under test.
        live_states.set_spawn_time_for_test(
            slot_id,
            boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
        );
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

    /// Every way a slot can prove it came up, and the one shape that proves
    /// it did not. This predicate decides whether an app-reported pane death
    /// is treated as a death or as a never-started spawn, and only the
    /// latter feeds the spawn-capability breaker — so a false positive here
    /// would let a genuinely crashed worker trip the fleet, and a false
    /// negative reproduces the 2026-07 churn.
    #[test]
    fn slot_never_started_requires_the_total_absence_of_proof_of_life() {
        let live_states = LiveWorkerStateRegistry::new();
        register_slot_zero_pid(&live_states, 1, "exec-1", "wi-1");
        let pristine = live_states.get(1).expect("slot 1");
        assert!(
            slot_never_started(&pristine),
            "no pid, no hook event, still Spawning is the never-started shape",
        );

        let with_pid = LiveWorkerState {
            shell_pid: 4242,
            ..pristine.clone()
        };
        assert!(!slot_never_started(&with_pid), "a reported shell pid is proof of life");

        let with_event = LiveWorkerState {
            last_event_at: Some("2026-07-31T00:00:00Z".to_owned()),
            ..pristine.clone()
        };
        assert!(!slot_never_started(&with_event), "a hook event is proof of life");

        let progressed = LiveWorkerState {
            activity: WorkerActivity::Working,
            ..pristine.clone()
        };
        assert!(
            !slot_never_started(&progressed),
            "activity past Spawning is proof of life",
        );
    }

    /// A never-started spawn reported to us as a pane death must not be
    /// narrated as a death. The reason text is the only durable explanation
    /// an operator gets, and "the worker pane died" for a pane that never
    /// came up sent every reader of the 2026-07 incident looking for a
    /// process that had never existed.
    #[test]
    fn pane_death_before_start_is_narrated_as_never_started() {
        let (reason, audit, stage) = reap_narrative(
            &ReapCause::PaneDiedBeforeStart {
                detail: "surface failed to attach",
            },
            "exec-1",
        );
        assert_eq!(stage, Stage::PaneDeathBeforeStart);
        assert!(
            reason.starts_with("pane-death-before-start:"),
            "reason must be greppable by cause; got: {reason}",
        );
        assert!(
            reason.contains("no worker process ever existed"),
            "reason must say no process existed, not that one died; got: {reason}",
        );
        assert!(
            reason.contains("surface failed to attach") && audit.contains("surface failed to attach"),
            "both surfaces must carry the app's observation verbatim",
        );
    }

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
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
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
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "a slot with a reported pid must not be reaped here");
        assert_eq!(outcome.skipped.has_pid, 1);
        assert!(sink.events().await.is_empty());
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    }

    /// A pid-less slot that has emitted at least one hook event is proof
    /// of life and must not be reaped.
    ///
    /// The setup records the driver signal alongside `apply_event` because
    /// that is what the production hook ingress does — `dispatch_live_worker_state`
    /// calls `record_driver_signal` before it resolves the slot and calls
    /// `apply_event`. Driving `apply_event` alone would be a hook that
    /// arrived without arriving.
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
        live_states.record_driver_signal(&execution_id, crate::live_worker_state::DriverSignalKind::HookEvent);
        live_states.apply_event(
            1,
            &WorkerEvent::SessionStart {
                session_id: "s".to_owned(),
                source: boss_protocol::SessionStartSource::Resume,
                model: None,
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
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
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

        let execution_id = create_execution_started_now(&db, &work_item_id);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);

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
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0, "grace period must prevent reaping fresh dispatches");
        assert_eq!(outcome.skipped.grace, 1);
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
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.reaped, 0);
        assert_eq!(outcome.skipped.not_spawning, 1);
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
        // idempotency (already paused → no second signal). This test exercises
        // the pause path, so it must opt in explicitly — `with_config`'s
        // default is now the config-driven `false`.
        let spawn_health = SpawnHealthTracker::with_config(3, 300).with_breaker_enabled(true);
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
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
        coordinator.pause_dispatch(
            now.max(0) as u64,
            crate::coordinator::DispatchPauseOrigin::Operator,
            boss_protocol::PauseReason::new("test: operator pause").unwrap(),
        );
        assert!(
            coordinator.dispatch_pause_exempts_reviews(),
            "precondition: operator pause exempts reviews"
        );

        // This test exercises the pause-escalation path, so it must opt in
        // explicitly — `with_config`'s default is now the config-driven `false`.
        let spawn_health = SpawnHealthTracker::with_config(3, 300).with_breaker_enabled(true);
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &NoopCube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
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
        let ctx = SpawnReapCtx::builder()
            .work_db(db.as_ref())
            .coordinator(coordinator.clone())
            .dispatch_events(sink.as_ref())
            .reaper(reaper.as_ref())
            .spawn_health(&spawn_health)
            .cube_client(&NoopCube)
            .build();
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

    // ─── driver-start verification (the 2026-07-30 class) ────────────────────

    /// Drive one full sweep pass and hand back everything the driver-start
    /// assertions need.
    async fn run_pass(
        db: &Arc<WorkDb>,
        live_states: &LiveWorkerStateRegistry,
        coordinator: &Arc<ExecutionCoordinator>,
        cube: &RecordingCube,
    ) -> (SpawnAckSweepOutcome, Arc<RecordingDispatchEventSink>) {
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        let outcome = run_one_pass(
            db.as_ref(),
            live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            cube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;
        (outcome, sink)
    }

    /// The incident, reproduced end to end.
    ///
    /// A pane spawned, the app reported a real foreground shell pid, and no
    /// driver-originated signal ever arrived. Before this check existed the
    /// positive pid made the slot invisible to every sweep and it held its
    /// slot and cube lease indefinitely with no attention item.
    ///
    /// Asserts all four things the reap must do: orphan the execution,
    /// release the pool slot, release the cube workspace lease, and raise an
    /// attention item.
    #[tokio::test]
    async fn driver_start_timeout_reaps_pane_whose_driver_never_started() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        // `create_spawned_execution` records the post-spawn shape including
        // the cube lease (`lease-1`) whose release is the point of the test.
        let execution_id = create_spawned_execution(&db, &work_item_id, 92697);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 92697, false);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.driver_start_reaped, 1,
            "a pane with a live shell pid and no driver signal must be reaped",
        );
        assert_eq!(
            outcome.reaped, 0,
            "pass 1 must not also claim it — the pid routes it to pass 2",
        );

        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned,
        );
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the worker slot must be released, not held",
        );
        assert_eq!(
            cube.released_lease_ids(),
            vec!["lease-1".to_owned()],
            "the cube workspace lease must be released, not held",
        );

        let attentions = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(attentions.len(), 1, "the reap must raise exactly one attention item");
        assert_eq!(attentions[0].kind, DRIVER_START_ATTENTION_KIND);
        assert!(
            attentions[0].body_markdown.contains("92697"),
            "the attention body must name the misleading shell pid; got: {:?}",
            attentions[0].body_markdown,
        );

        let events = sink.events().await;
        let reaps: Vec<_> = events.iter().filter(|e| e.stage == "driver_start_timeout").collect();
        assert_eq!(reaps.len(), 1);
        assert_eq!(reaps[0].details["shell_pid"], serde_json::json!(92697));
        assert_eq!(
            reaps[0].details["threshold_secs"],
            serde_json::json!(DRIVER_START_GRACE_SECS),
        );
    }

    /// The detection must not inherit `mark_stalled_spawns`'s
    /// `Capability::AwaitingInputSignal` exemption.
    ///
    /// Runs the identical scenario for a capability-declaring driver (claude)
    /// and a non-declaring one (grok) and asserts both are reaped. Grok's
    /// omission of the capability is what made the real occurrence invisible.
    #[tokio::test]
    async fn driver_start_timeout_fires_regardless_of_awaiting_input_capability() {
        for awaiting_input_capable in [true, false] {
            let (_dir, db) = open_db();
            let product_id = create_product(&db);
            let work_item_id = create_active_chore(&db, &product_id, "test chore");
            let db = Arc::new(db);

            let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
            let live_states = Arc::new(LiveWorkerStateRegistry::new());
            register_slot_with_live_shell(
                &live_states,
                1,
                &execution_id,
                &work_item_id,
                4242,
                awaiting_input_capable,
            );

            // Let `mark_stalled_spawns` run first, exactly as the engine does.
            // For the capable driver it promotes the slot to `WaitingForInput`
            // and synthesizes a `last_event_at`; for the incapable one it
            // declines. Neither may hide the slot from driver-start
            // verification.
            live_states.mark_stalled_spawns(
                boss_engine_utils::epoch_time::now_epoch_secs(),
                crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS,
            );

            let coordinator = make_coordinator(db.clone(), 1);
            coordinator.worker_pool().claim_worker(&execution_id, None).await;

            let cube = RecordingCube::default();
            let (outcome, _sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

            assert_eq!(
                outcome.driver_start_reaped, 1,
                "driver-start verification must fire with awaiting_input_capable={awaiting_input_capable}",
            );
            assert_eq!(
                db.get_execution(&execution_id).unwrap().status,
                ExecutionStatus::Orphaned,
                "awaiting_input_capable={awaiting_input_capable}",
            );
        }
    }

    /// A slot `mark_stalled_spawns` has promoted out of `Spawning` must still
    /// be reached. Pass 1 filters on `activity == Spawning`; if pass 2 shared
    /// that filter, the promotion would be an escape hatch.
    ///
    /// Also pins the reason the promotion is not itself proof of life: it
    /// writes `last_event_at` from engine-side inference, and that timestamp
    /// must not satisfy driver-start verification.
    #[tokio::test]
    async fn promoted_slot_is_still_subject_to_driver_start_verification() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 555);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 555, true);

        let promoted = live_states.mark_stalled_spawns(
            boss_engine_utils::epoch_time::now_epoch_secs(),
            crate::live_worker_state::STALLED_SPAWN_THRESHOLD_SECS,
        );
        assert_eq!(promoted, vec![1], "precondition: the slot leaves Spawning");
        let state = live_states.get(1).unwrap();
        assert_eq!(state.activity, WorkerActivity::WaitingForInput);
        assert!(
            state.last_event_at.is_some(),
            "precondition: the promotion synthesizes a last_event_at",
        );
        assert!(
            live_states.driver_signal_at(1).is_none(),
            "the synthesized last_event_at must NOT count as driver evidence",
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, _sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.driver_start_reaped, 1,
            "leaving Spawning must not exempt a slot from driver-start verification",
        );
    }

    /// No false positives: a worker whose driver DID start
    /// is never touched, however long it then runs without further events —
    /// the driver-start signal is first-write-wins and permanent.
    #[tokio::test]
    async fn a_driver_that_signalled_is_never_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 777);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 777, false);

        // The driver reported in exactly once, long ago.
        assert_eq!(
            live_states.record_driver_signal(&execution_id, crate::live_worker_state::DriverSignalKind::HookEvent),
            Some(1),
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(outcome.driver_start_reaped, 0, "a started driver must never be reaped");
        assert_eq!(outcome.reaped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running,
            "the execution must be left exactly as it was",
        );
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the slot must NOT be released out from under a working worker",
        );
        assert!(
            cube.released_lease_ids().is_empty(),
            "the cube lease must NOT be released out from under a working worker",
        );
        assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
        assert!(sink.events().await.iter().all(|e| e.stage != "driver_start_timeout"));
    }

    /// Re-adoption then sweep: the engine re-registers an already-running
    /// worker, and the sweep must leave it entirely alone.
    ///
    /// `readopt_live_worker` restores the row to `waiting_human` and
    /// re-registers the slot, which stamps `spawned_at` with the current
    /// time for a process that has been running for however long. The
    /// `redispatch_guard` trigger carries no driver-originated evidence at
    /// all (it fires off a recorded-*shell*-pid probe), and a worker parked
    /// at `waiting_human` emits no further hook by definition — so without
    /// the re-adoption exemption nothing would ever supply the missing
    /// proof and this pass would reap a live worker one grace window later:
    /// pane killed by process group, workspace torn down, cube lease
    /// force-released.
    #[tokio::test]
    async fn a_readopted_live_worker_is_not_reaped_as_a_never_started_driver() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 92697);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Exactly what `readopt_live_worker` does for the pid-only trigger.
        live_states.register_readoption(
            1,
            execution_id.as_str(),
            "grok-4.5",
            92697,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.clone(),
            }),
            false,
            crate::live_worker_state::LiveSpawnRouting::none(),
            crate::live_worker_state::ReadoptionEvidence::LiveShellPid,
        );
        // Age the re-registration past every window under test.
        live_states.set_spawn_time_for_test(
            1,
            boss_engine_utils::epoch_time::now_epoch_secs() - (DRIVER_START_GRACE_SECS + 60),
        );
        assert!(
            live_states.driver_signal_at(1).is_none(),
            "precondition: a pid-triggered re-adoption records no driver proof",
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.driver_start_reaped, 0,
            "a re-adopted worker is not a spawn, so it has no driver start to verify",
        );
        assert_eq!(outcome.reaped, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running,
            "the re-adopted execution must be left exactly as re-adoption restored it",
        );
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the slot must NOT be released out from under a re-adopted worker",
        );
        assert!(
            cube.released_lease_ids().is_empty(),
            "the cube lease must NOT be force-released out from under a re-adopted worker",
        );
        assert!(db.list_attention_items(&execution_id).unwrap().is_empty());
        assert!(sink.events().await.iter().all(|e| e.stage != "driver_start_timeout"));
    }

    /// A driver still inside its grace window is left alone, so a merely-slow
    /// start is never reaped.
    #[tokio::test]
    async fn driver_start_verification_respects_its_grace_window() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 888);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 888, false);
        // Spawned well inside the window: no driver signal yet, but too early
        // to conclude anything.
        live_states.set_spawn_time_for_test(1, boss_engine_utils::epoch_time::now_epoch_secs() - 5);

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, _sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(outcome.driver_start_reaped, 0, "a fresh spawn must be given its window");
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Running,
        );
    }

    /// Pass 1's proof-of-life test is now the driver signal, not
    /// `last_event_at`. A zero-pid slot carrying only a synthesized
    /// `last_event_at` must still be reaped rather than skipped.
    #[tokio::test]
    async fn pass_one_no_longer_treats_a_synthesized_timestamp_as_proof_of_life() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
        // An engine-written timestamp with no driver behind it.
        live_states.set_last_event_at_for_test(1, "2026-07-30T05:47:45Z");
        assert!(live_states.driver_signal_at(1).is_none());

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, _sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.reaped, 1,
            "only a driver-originated signal may suppress the spawn-ack reap",
        );
        assert_eq!(outcome.skipped.has_driver_signal, 0);
    }
}
