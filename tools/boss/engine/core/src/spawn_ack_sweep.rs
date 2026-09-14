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
//! Both passes funnel into [`reap_never_started_spawn`], which first
//! consults the liveness veto (below), then marks the
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
//! ## The third incident: live workers reaped as driver-start timeouts
//!
//! On 2026-09-13 dispatch resumed after a 19-hour pause and admitted six
//! Codex executions inside one ~7-second window. Every pane and shell came
//! up in 4–7 seconds. Four of the workers were then reaped by pass 2 at
//! 300 s, and the breaker tripped and paused dispatch, reviews included.
//! All four rollouts were on disk — 272 KB to 974 KB, hundreds of
//! thousands of tokens processed — and were still being appended to at the
//! moment of the reap; one was mid-`CommandExecution` 1.6 s before it was
//! killed.
//!
//! The chain was: the progress ingress's rollout discovery never attached
//! the rollouts (three landed seconds after its window; one was present
//! with 43 s of margin and was never matched — see
//! [`crate::agent_jsonl_discovery`] for what that loop now reports), so no
//! ingress event ever reached the engine, so `driver_signal_at` was never
//! set, so pass 2 concluded "the driver binary never started" and reaped.
//! Nothing between discovery and the reap ever looked at the filesystem.
//!
//! Two things changed here as a result:
//!
//! **The liveness veto.** [`reap_never_started_spawn`] — the one reap every
//! cause funnels through — now asks
//! [`crate::transcript_liveness::probe_transcript_liveness`] before it does
//! anything. The veto itself only applies to [`ReapCause::vetoable`] causes —
//! [`ReapCause::SpawnAckTimeout`] and [`ReapCause::DriverStartTimeout`], the
//! two the periodic sweep infers from silence. For those, a transcript for
//! the execution on disk (a rollout newer than the pre-spawn baseline under
//! the run's ingress root, or the run row's recorded transcript path scoped
//! to this incarnation) is driver-originated evidence in its own right: the
//! reap records it as a driver signal
//! ([`crate::live_worker_state::DriverSignalKind::CorrelatedTranscript`] or
//! [`crate::live_worker_state::DriverSignalKind::CorrelatedTranscriptUnattachable`],
//! permanent, first-write-wins) and returns [`ReapOutcome::Vetoed`]. An
//! answer that cannot be established — an unreadable checkpoint, a root
//! that will not scan — is [`ReapOutcome::LivenessUndeterminable`]: logged
//! at error level with what could not be read, and NOT reaped, because an
//! unreadable answer is not "absent" and a false reap destroys real work.
//! Only [`crate::transcript_liveness::TranscriptLiveness::Absent`] lets a
//! reap proceed, and the probe's summary then becomes part of the orphan
//! reason so the record says what was checked.
//!
//! [`ReapCause::AppNack`] and [`ReapCause::PaneDiedBeforeStart`] are
//! different in kind: the app has positively reported that the pane failed
//! to spawn or is gone. A transcript on disk answers "did the driver ever
//! run?", not "does the pane still exist?" — it does not contradict the
//! app's report, so it is never grounds to refuse those two reaps. The probe
//! still runs for them (its summary is diagnostic value in the orphan
//! reason), but it never blocks the reap and never records a permanent
//! driver signal on that path — recording one for a pane the app itself
//! says is gone would leave a `pid<=0`/`Spawning` slot no other sweep can
//! reclaim (neither sweep pass, `dead_pid_sweep`, nor `stale_worker_sweep`
//! — see this module's own doc above).
//!
//! **The failure class.** Pass 1 and pass 2 observe different things, and
//! the breaker used to announce both as "failed to spawn a worker shell".
//! Every reap now records its [`crate::spawn_health::SpawnFailureClass`]
//! on the breaker evidence, and the pause reason and attention item are
//! composed from the classes actually observed. Pass 2's own wording — the
//! log line, the orphan reason, the per-execution attention item — states
//! what was observed (a pane and shell came up; no driver-originated
//! signal arrived; what the liveness probe found) rather than the
//! inference "the driver binary never started", which the incident showed
//! can be false.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::stale_worker_sweep`]).

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{CreateAttentionItemInput, LiveWorkerState, WorkExecution, WorkerActivity};

use crate::agent_jsonl_progress::{DiscoveryVerdict, IngressCheckpoint, IngressCheckpointStore};
use crate::coordinator::{CubeClient, ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::{DriverSignalKind, DriverStartExpectation, LiveWorkerStateRegistry};
use crate::spawn_health::{
    SpawnFailureClass, SpawnHealthTracker, maybe_admit_recovery_probe, trip_spawn_capability_circuit,
};
use crate::transcript_liveness::{TranscriptLiveness, probe_transcript_liveness};
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
pub(crate) fn slot_never_started(live_states: &LiveWorkerStateRegistry, state: &LiveWorkerState) -> bool {
    live_states.driver_start_expectation(state.slot_id) != Some(DriverStartExpectation::Readopted)
        && state.shell_pid <= 0
        && state.last_event_at.is_none()
        && state.activity == WorkerActivity::Spawning
}

/// Kind string for the attention item raised when a spawn produced a
/// pane but no driver. Stable — operator tooling pins it.
pub const DRIVER_START_ATTENTION_KIND: &str = "worker_driver_never_started";

/// Kind string for the attention item raised when the liveness veto's
/// answer has stayed [`crate::transcript_liveness::TranscriptLiveness::Undeterminable`]
/// for [`UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD`] consecutive passes.
/// Stable — operator tooling pins it.
pub const LIVENESS_UNDETERMINABLE_ATTENTION_KIND: &str = "worker_liveness_undeterminable";

/// Consecutive `LivenessUndeterminable` reap outcomes for the same
/// execution before [`raise_liveness_undeterminable_attention`] fires.
///
/// Several causes of `Undeterminable` do not heal on their own — a cube
/// workspace already reclaimed, a stored checkpoint that will not
/// deserialize — so an execution stuck here is held (its slot, its cube
/// lease, its work item) indefinitely, re-probed every sweep pass, with
/// nothing surfacing it beyond an `error`-level log line. Five passes at the
/// sweep's ~60s cadence is a few minutes: long enough that a single
/// transient read failure never trips it, short enough that a genuinely
/// stuck slot does not go unnoticed for the life of the run the way it did
/// before this existed.
pub const UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD: u32 = 5;

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
    /// Reaps (either pass) refused because a transcript for the execution
    /// exists on disk — see the module doc's liveness veto. Each of these
    /// is a worker that would have been killed alive.
    pub vetoed: usize,
    /// Reaps (either pass) refused because liveness could not be
    /// established. Logged at error level with the reason; the slot is
    /// re-examined next pass.
    pub liveness_undeterminable: usize,
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
    /// Slot pre-dates this engine process (readopted at boot or after a
    /// terminal-execution contradiction), so it is not a new spawn awaiting
    /// an ack and pass 1's ack-timeout question does not apply to it.
    pub readopted: usize,
}

impl crate::sweep_loop::SweepOutcome for SpawnAckSweepOutcome {
    fn has_activity(&self) -> bool {
        self.reaped > 0 || self.driver_start_reaped > 0 || self.vetoed > 0 || self.liveness_undeterminable > 0
    }

    fn log(&self) {
        tracing::info!(
            reaped = self.reaped,
            driver_start_reaped = self.driver_start_reaped,
            vetoed = self.vetoed,
            liveness_undeterminable = self.liveness_undeterminable,
            has_pid_skipped = self.skipped.has_pid,
            has_driver_signal_skipped = self.skipped.has_driver_signal,
            grace_skipped = self.skipped.grace,
            readopted_skipped = self.skipped.readopted,
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
        .live_states(live_states)
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

        // A re-adopted slot represents a worker that existed before this
        // engine process. It is not a new spawn awaiting its first ack, even
        // when its durable shell-pid probe could not produce a positive pid.
        if live_states.driver_start_expectation(state.slot_id) == Some(DriverStartExpectation::Readopted) {
            outcome.skipped.readopted += 1;
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

        match reap_never_started_spawn(
            &ctx,
            &execution,
            state.slot_id,
            state.shell_pid,
            ReapCause::SpawnAckTimeout { grace_secs },
            now_epoch_secs,
        )
        .await
        {
            ReapOutcome::Reaped => outcome.reaped += 1,
            ReapOutcome::Vetoed => outcome.vetoed += 1,
            ReapOutcome::LivenessUndeterminable => outcome.liveness_undeterminable += 1,
            ReapOutcome::Skipped => {}
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

        // A re-adopted slot's in-memory `driver_signal_at` was seeded only
        // once, at adoption time, from the durable checkpoint. If that
        // one-shot read failed or found nothing yet (the checkpoint write
        // can race run-row creation — see `record_semantic_progress`), the
        // slot is stuck here with no further chance to recover: a worker
        // parked at `waiting_human` emits no more hooks by definition, so
        // the checkpoint is its only protection. Re-read the checkpoint
        // now, right before reaping, rather than trusting the one-shot
        // restore. An `Err` is treated as inconclusive — never as
        // permission to reap — because we cannot tell it apart from a real
        // checkpoint the read simply failed to fetch.
        if live_states.driver_start_expectation(candidate.slot_id) == Some(DriverStartExpectation::Readopted) {
            match work_db.get_run_semantic_progress_checkpoint(execution_id) {
                Ok(Some(checkpoint)) => {
                    live_states.seed_semantic_progress(candidate.slot_id, &checkpoint);
                    tracing::info!(
                        execution_id,
                        slot_id = candidate.slot_id,
                        "driver-start check: re-read the durable checkpoint and found driver-start proof \
                         that the one-shot adoption-time restore missed; skipping the reap",
                    );
                    continue;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(
                        execution_id,
                        slot_id = candidate.slot_id,
                        error = %format!("{err:#}"),
                        "driver-start check: could not re-read the semantic-progress checkpoint for a \
                         re-adopted slot; treating as inconclusive and skipping this pass's reap rather \
                         than reaping on an unreadable checkpoint",
                    );
                    continue;
                }
            }
        }

        let file_ingress = file_ingress_state(work_db, execution_id);
        let pane_observation = if candidate.shell_pid > 0 {
            "a pane spawned (shell pid reported)"
        } else {
            "a pane spawned with no shell pid ever reported"
        };
        tracing::warn!(
            execution_id,
            work_item_id = %execution.work_item_id,
            slot_id = candidate.slot_id,
            shell_pid = candidate.shell_pid,
            activity = candidate.activity.as_str(),
            silent_secs = candidate.silent_secs,
            threshold_secs = driver_start_grace_secs,
            reading = %driver_start_reading(file_ingress.as_ref()),
            "driver-start timeout: {pane_observation} and no driver-originated signal — no hook event, \
             no transcript path, no ingress event — has been observed since. Consulting the transcript \
             on disk before deciding whether the driver ever ran.",
        );

        match reap_never_started_spawn(
            &ctx,
            &execution,
            candidate.slot_id,
            candidate.shell_pid,
            ReapCause::DriverStartTimeout {
                grace_secs: driver_start_grace_secs,
                silent_secs: candidate.silent_secs,
                activity: candidate.activity.as_str(),
                file_ingress,
            },
            now_epoch_secs,
        )
        .await
        {
            ReapOutcome::Reaped => outcome.driver_start_reaped += 1,
            ReapOutcome::Vetoed => outcome.vetoed += 1,
            ReapOutcome::LivenessUndeterminable => outcome.liveness_undeterminable += 1,
            ReapOutcome::Skipped => {}
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
    /// Where a transcript found by the liveness veto is recorded as the
    /// run's driver-start proof, so the slot is never a candidate again.
    pub live_states: &'a LiveWorkerStateRegistry,
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
    /// A pane came up — possibly with a live shell pid — and no
    /// driver-originated signal was observed within the window. That is
    /// what was seen; whether the driver never executed or its signal never
    /// reached the engine is what the liveness veto in
    /// [`reap_never_started_spawn`] decides. Unlike the two above, this one
    /// also raises a per-execution attention item: nothing else in Boss
    /// surfaces it.
    DriverStartTimeout {
        grace_secs: i64,
        silent_secs: i64,
        activity: &'static str,
        /// What the run's file-ingress checkpoint says, when it has one.
        file_ingress: Option<FileIngressState>,
    },
}

/// The file-ingress checkpoint's account of a run that produced no signal,
/// rendered for the reap narrative and the dispatch event.
#[derive(Clone, Debug)]
pub(crate) struct FileIngressState {
    /// One sentence for the orphan reason / audit note / attention body.
    pub summary: String,
    /// The same facts, structured, for `details.file_ingress`.
    pub details: serde_json::Value,
}

/// Read the run's durable ingress checkpoint and say what it implies about
/// the missing driver signal. `None` when the run has no file ingress (a
/// hook-socket driver) or no record at all; every other state — including
/// an unreadable record — is worth a sentence, because the default reading
/// of a driver-start timeout ("the binary never ran") is the one that was
/// wrong in the incident.
pub(crate) fn file_ingress_state(work_db: &WorkDb, execution_id: &str) -> Option<FileIngressState> {
    let checkpoint = match work_db.load_ingress_checkpoint(execution_id) {
        Ok(Some(checkpoint)) => checkpoint,
        Ok(None) => return None,
        Err(err) => {
            return Some(FileIngressState {
                summary: format!("the run's file-ingress checkpoint could not be read ({err})"),
                details: serde_json::json!({ "state": "unreadable", "error": err }),
            });
        }
    };
    match checkpoint {
        IngressCheckpoint::NotFileIngress => None,
        IngressCheckpoint::Armed { discovery: None, .. } => Some(FileIngressState {
            summary: "the engine's file ingress was armed but never attached to a rollout and recorded \
                      no discovery verdict"
                .to_owned(),
            details: serde_json::json!({ "state": "armed" }),
        }),
        IngressCheckpoint::Armed {
            discovery: Some(record),
            ..
        } => {
            let summary = match record.verdict {
                DiscoveryVerdict::Overdue => format!(
                    "the engine's file ingress never attached to a rollout: discovery was overdue at \
                     {}s, recorded {}s before this reap ({} rollout-shaped file(s) that did not correlate to this run); discovery was still looking, so the \
                     driver may have started and run unobserved",
                    record.waited_secs,
                    boss_engine_utils::epoch_time::now_epoch_secs()
                        .saturating_sub(record.at_epoch_secs)
                        .max(0),
                    record.rejected_candidates
                ),
                DiscoveryVerdict::Failed => format!(
                    "the engine's file ingress never attached to a rollout: discovery failed after {}s \
                     ({}), so the driver may have started and run unobserved",
                    record.waited_secs, record.reason
                ),
            };
            Some(FileIngressState {
                summary,
                details: serde_json::json!({
                    "state": "armed",
                    "discovery": record,
                }),
            })
        }
        IngressCheckpoint::Attached { path, session_id, .. } => Some(FileIngressState {
            summary: format!(
                "the engine's file ingress attached to {} (session {session_id}) but no event was ever \
                 dispatched from it",
                path.display()
            ),
            details: serde_json::json!({
                "state": "attached",
                "path": path.display().to_string(),
                "session_id": session_id,
            }),
        }),
    }
}

/// The clause that names the reading of a driver-start timeout: what the
/// file ingress recorded, or — with no file ingress — that the binary most
/// likely never ran.
fn driver_start_reading(file_ingress: Option<&FileIngressState>) -> String {
    match file_ingress {
        Some(state) => state.summary.clone(),
        None => "no file-ingress record exists for this run, so the driver binary most likely never \
                 started"
            .to_owned(),
    }
}

impl ReapCause<'_> {
    /// Which failure shape this cause observed — the breaker composes its
    /// pause reason from these, so the wording an operator reads matches
    /// what each reap actually saw.
    pub(crate) fn failure_class(&self) -> SpawnFailureClass {
        match self {
            ReapCause::SpawnAckTimeout { .. } | ReapCause::AppNack { .. } | ReapCause::PaneDiedBeforeStart { .. } => {
                SpawnFailureClass::NoShell
            }
            ReapCause::DriverStartTimeout { .. } => SpawnFailureClass::ShellWithoutDriverSignal,
        }
    }

    /// Whether the liveness veto applies to this cause.
    ///
    /// Only the two causes the periodic sweep *infers from silence*
    /// (`SpawnAckTimeout`, `DriverStartTimeout`) ask "did the driver ever
    /// run?" — a question a transcript on disk actually answers, and can
    /// therefore contradict. `AppNack` and `PaneDiedBeforeStart` are the app
    /// *reporting* that the pane failed or is gone: positive evidence a
    /// transcript's mere existence does not contradict, so vetoing those
    /// reaps on a transcript would refuse to act on the app's own report and
    /// leave the slot in the exact `pid<=0`/`Spawning` shape no other sweep
    /// can reclaim (see the module doc).
    pub(crate) fn vetoable(&self) -> bool {
        matches!(
            self,
            ReapCause::SpawnAckTimeout { .. } | ReapCause::DriverStartTimeout { .. }
        )
    }
}

/// What [`reap_never_started_spawn`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReapOutcome {
    /// The execution was orphaned, its pane torn down, its slot and lease
    /// released, and the breaker fed.
    Reaped,
    /// A transcript for the execution exists on disk, so the driver ran.
    /// Nothing was touched; the transcript was recorded as the run's
    /// driver-start signal so no pass asks again.
    Vetoed,
    /// Liveness could not be established (the reason was logged at error
    /// level). Nothing was touched; the slot is re-examined next pass.
    LivenessUndeterminable,
    /// The execution was already terminal, or the orphan write failed.
    Skipped,
}

/// The operator-facing narrative for one never-started reap: the orphan
/// reason recorded on the run, the `[engine-reconcile]` audit line appended
/// to the work item's description, and the dispatch stage emitted.
///
/// `liveness` is the transcript probe's summary — what the reap checked on
/// disk before it proceeded — and is appended to both texts so the record
/// says what was verified, not only what was inferred.
///
/// Pure and free-standing so each cause's story is unit-testable without a
/// DB, a pool, or an app session. That matters most for the reason text: it
/// is the only durable explanation an operator gets for why an execution
/// vanished, and a cause whose reason describes the wrong event (a "death"
/// for a pane that never came up) is indistinguishable from no explanation
/// at all.
fn reap_narrative(
    cause: &ReapCause<'_>,
    execution_id: &str,
    liveness: &str,
    shell_pid: i32,
) -> (String, String, Stage) {
    let (reason, audit, stage) = reap_narrative_for_cause(cause, execution_id, shell_pid);
    (
        format!("{reason}; liveness probe: {liveness}"),
        format!("{audit} Liveness probe before the reap: {liveness}."),
        stage,
    )
}

fn reap_narrative_for_cause(cause: &ReapCause<'_>, execution_id: &str, shell_pid: i32) -> (String, String, Stage) {
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
            file_ingress,
            ..
        } => {
            // `unverified_driver_starts` is deliberately blind to
            // `shell_pid` (see the module doc), so pass 2 can reap a
            // candidate that never reported one at all — a readopted slot,
            // or one pass 1 skipped for another reason. Asserting "a pane
            // and shell came up" for that shape is the mirror-image of the
            // mislabel this PR fixes elsewhere: say what was actually
            // observed.
            let pane_observation = if shell_pid > 0 {
                "a pane and shell came up"
            } else {
                "a pane was spawned but no shell pid was ever reported"
            };
            let reading = driver_start_reading(file_ingress.as_ref());
            (
                format!(
                    "driver-start-timeout: {pane_observation} but no driver-originated signal (hook \
                     event, transcript path, or progress-ingress event) was observed within {grace_secs}s \
                     (silent for {silent_secs}s) and no transcript exists on disk; {reading}"
                ),
                format!(
                    "driver-start timeout (exec {execution_id}) detected — {pane_observation} but no \
                     hook event, transcript path, or progress-ingress event was observed within \
                     {grace_secs}s and no transcript exists on disk; {reading}; worker slot and cube workspace lease \
                     released, chore reset to todo for redispatch."
                ),
                Stage::DriverStartTimeout,
            )
        }
    }
}

/// Reap a `Spawning` slot that never produced a live shell: mark the execution
/// orphaned, back up any uncommitted work, append an `[engine-reconcile]`
/// audit line, tear down the (possibly ghost) app pane, release the pool slot,
/// emit a dispatch event, and feed the spawn-capability circuit breaker —
/// tripping it when too many DISTINCT work items fail in the window. Returns
/// a [`ReapOutcome`]: `Reaped` when all of the above happened; `Vetoed` when
/// the liveness veto refused the reap because a transcript proves the driver
/// ran; `LivenessUndeterminable` when the veto's question could not be
/// answered at all; `Skipped` when the execution was already terminal, or
/// the orphan write failed.
///
/// Shared by [`run_one_pass`] (the 60s timeout path),
/// [`crate::app::sessions::handle_report_worker_spawn_failed`] (the immediate
/// NACK path), and [`crate::app::sessions::handle_worker_pane_died`] when the
/// reported "death" turns out to be a pane that never came up at all
/// ([`slot_never_started`]) — so all three do exactly the same thing, and in
/// particular all three feed the breaker and all three consult the liveness
/// probe. The only difference is `cause`.
///
/// ## The liveness veto
///
/// Before anything is written, the execution's transcript is looked for on
/// disk ([`probe_transcript_liveness`], run via [`tokio::task::spawn_blocking`]
/// so the scan never runs inline on the async runtime). Whether a present or
/// undeterminable answer actually blocks the reap depends on
/// [`ReapCause::vetoable`] — see that method's doc and the module doc's
/// section on the veto. For a vetoable cause, a present transcript is
/// recorded as the run's driver-start signal and the reap returns
/// [`ReapOutcome::Vetoed`]; an undeterminable answer returns
/// [`ReapOutcome::LivenessUndeterminable`] and touches nothing. For a
/// non-vetoable cause (an app-reported NACK or pane death), the probe still
/// runs and its summary is folded into the orphan reason for diagnostic
/// value, but neither answer blocks the reap or is recorded as a driver
/// signal. Only a confirmed absence — or a non-vetoable cause — proceeds to
/// the reap, and the probe's summary is always written into the orphan
/// reason and audit line so the record shows what was checked.
pub(crate) async fn reap_never_started_spawn(
    ctx: &SpawnReapCtx<'_>,
    execution: &WorkExecution,
    slot_id: u8,
    shell_pid: i32,
    cause: ReapCause<'_>,
    now_epoch_secs: i64,
) -> ReapOutcome {
    let execution_id = execution.id.as_str();
    let work_item_id = execution.work_item_id.as_str();

    let liveness = {
        // `WorkDb::clone` explicitly, not `ctx.work_db.clone()`: `ctx.work_db`
        // is already `&WorkDb`, and the latter spelling resolves to cloning
        // the reference itself (always `Clone`), not the owned `WorkDb` the
        // `'static` closure below needs.
        let work_db = WorkDb::clone(ctx.work_db);
        let execution_id_owned = execution_id.to_owned();
        let started_epoch = execution.started_epoch();
        match tokio::task::spawn_blocking(move || {
            probe_transcript_liveness(&work_db, &execution_id_owned, now_epoch_secs, started_epoch)
        })
        .await
        {
            Ok(liveness) => liveness,
            Err(join_err) => {
                tracing::error!(
                    execution_id,
                    work_item_id,
                    slot_id,
                    error = %join_err,
                    "liveness probe task panicked or was cancelled; treating as undeterminable rather \
                     than absent",
                );
                TranscriptLiveness::Undeterminable {
                    reasons: vec![format!("the liveness probe task did not complete: {join_err}")],
                    checked: Vec::new(),
                }
            }
        }
    };

    if cause.vetoable() {
        match &liveness {
            TranscriptLiveness::Present {
                source,
                path,
                age_secs,
                bytes,
                discovery_would_attach,
                detail,
            } => {
                let kind = if *discovery_would_attach {
                    DriverSignalKind::CorrelatedTranscript
                } else {
                    DriverSignalKind::CorrelatedTranscriptUnattachable
                };
                let recorded_slot = ctx.live_states.record_driver_signal(execution_id, kind);
                match recorded_slot {
                    Some(recorded_slot) => tracing::error!(
                        execution_id,
                        work_item_id,
                        slot_id,
                        shell_pid,
                        stage = cause.failure_class().as_str(),
                        source = source.as_str(),
                        transcript = %path.display(),
                        age_secs,
                        bytes,
                        signal_kind = kind.as_str(),
                        recorded_slot,
                        detail,
                        "REFUSING to reap as a never-started spawn: a transcript for this execution exists \
                         on disk, so the driver ran. No driver-originated signal reached the engine through \
                         the hook or progress ingress — that is an observation gap to fix, not a dead \
                         worker. Recorded the transcript as the run's driver-start signal.",
                    ),
                    None => tracing::error!(
                        execution_id,
                        work_item_id,
                        slot_id,
                        shell_pid,
                        stage = cause.failure_class().as_str(),
                        source = source.as_str(),
                        transcript = %path.display(),
                        age_secs,
                        bytes,
                        signal_kind = kind.as_str(),
                        detail,
                        "REFUSING to reap as a never-started spawn: a transcript for this execution exists \
                         on disk, so the driver ran. No live slot is registered for this run any more, so \
                         the transcript could NOT be recorded as a driver signal — the run will be \
                         re-examined on the next pass rather than reaped.",
                    ),
                }
                return ReapOutcome::Vetoed;
            }
            TranscriptLiveness::Undeterminable { reasons, checked } => {
                tracing::error!(
                    execution_id,
                    work_item_id,
                    slot_id,
                    shell_pid,
                    stage = cause.failure_class().as_str(),
                    reasons = %reasons.join("; "),
                    checked = %checked.join("; "),
                    "NOT reaping: could not establish whether a transcript exists for this execution. An \
                     unreadable answer is not an absent transcript; the slot will be re-examined on the \
                     next pass.",
                );
                if ctx
                    .live_states
                    .record_liveness_undeterminable(execution_id, UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD)
                    == Some(true)
                {
                    raise_liveness_undeterminable_attention(
                        ctx.work_db,
                        execution,
                        slot_id,
                        shell_pid,
                        reasons,
                        checked,
                    );
                }
                return ReapOutcome::LivenessUndeterminable;
            }
            TranscriptLiveness::Absent { .. } => {}
        }
    } else if liveness.vetoes_reap() {
        // `AppNack` / `PaneDiedBeforeStart`: the app itself is positive
        // evidence the pane failed or is gone, which a transcript's mere
        // existence does not contradict. Surfaced for diagnostic value only
        // — see `ReapCause::vetoable`'s doc for why this must never block
        // the reap or record a permanent driver signal.
        tracing::info!(
            execution_id,
            work_item_id,
            slot_id,
            shell_pid,
            stage = cause.failure_class().as_str(),
            liveness = %liveness,
            "never-started-spawn reap: the liveness probe found something before an app-reported cause; \
             proceeding with the reap regardless, since the app's own report is positive evidence the \
             pane is gone",
        );
    }
    let liveness_summary = liveness.to_string();

    let (orphan_reason, audit_note, stage) = reap_narrative(&cause, execution_id, &liveness_summary, shell_pid);

    if let Err(err) = ctx.work_db.mark_execution_orphaned(execution_id, &orphan_reason) {
        tracing::warn!(
            execution_id,
            ?err,
            "reap-never-started-spawn: failed to mark execution orphaned; skipping reap",
        );
        return ReapOutcome::Skipped;
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
    // An overdue or failed ingress does not prove the workspace was
    // unoccupied: a driver may have run unobserved. Release relies on the
    // pane teardown above preventing further pane-hosted worker progress,
    // not on missing signals. Keeping the lease after teardown would leave
    // it warmed by DB heartbeats until TTL, as in the 2026-07-30 incident. Mirrors
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
            file_ingress,
        } => {
            details["threshold_secs"] = serde_json::json!(grace_secs);
            details["silent_secs"] = serde_json::json!(silent_secs);
            details["activity"] = serde_json::json!(activity);
            details["file_ingress"] = file_ingress
                .as_ref()
                .map_or(serde_json::Value::Null, |state| state.details.clone());
            raise_driver_start_attention(
                ctx.work_db,
                execution,
                slot_id,
                shell_pid,
                *grace_secs,
                *silent_secs,
                file_ingress.as_ref(),
                &liveness_summary,
            );
        }
    }
    details["failure_class"] = serde_json::json!(cause.failure_class().as_str());
    details["liveness_probe"] = serde_json::json!(liveness_summary);
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
    ctx.spawn_health.record_evidence(
        crate::spawn_health::SpawnFailureEvidence::builder()
            .execution_id(execution_id)
            .work_item_id(work_item_id)
            .slot_id(slot_id.to_string())
            .shell_pid(shell_pid)
            .epoch_secs(now_epoch_secs)
            .class(cause.failure_class())
            .cause(stage.as_str())
            .observed(orphan_reason.as_str())
            .build(),
    );
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

    ReapOutcome::Reaped
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
    file_ingress: Option<&FileIngressState>,
    liveness: &str,
) {
    let execution_id = execution.id.as_str();
    let reading = driver_start_reading(file_ingress);
    let pid_note = if shell_pid > 0 && file_ingress.is_some() {
        format!("The pane reported shell pid `{shell_pid}`; this does not establish whether the driver ran.")
    } else if shell_pid > 0 {
        format!(
            "The pane reported shell pid `{shell_pid}`, which is why every pane-level check treated \
             this slot as healthy — a shell pid proves the pane hosts a shell, not that the driver \
             inside it started."
        )
    } else {
        "No shell pid was ever reported for this pane.".to_owned()
    };
    let advice = if file_ingress.is_some() {
        "Inspect the file-ingress checkpoint and rollout diagnostics to determine why no driver signal was observed."
    } else {
        "If this repeats for the same driver, the spawn command is most likely not reaching the driver binary at all — check how the command is delivered to the pane."
    };
    let title = if file_ingress.is_some() {
        format!("Worker produced no driver signal on slot {slot_id}")
    } else {
        format!("Worker driver never started on slot {slot_id}")
    };
    let body = format!(
        "**Observed (driver-start timeout, sweep pass 2):** a worker pane was spawned for execution \
         `{execution_id}` on slot {slot_id} and came up, but no driver-originated signal — no hook \
         event, no `transcript_path`, no progress-ingress event — was observed within {grace_secs}s \
         (silent for {silent_secs}s).\n\n\
         **Checked before reaping:** {liveness}.\n\n\
         **File ingress:** {reading}.\n\n\
         {pid_note}\n\n\
         The engine has reaped the execution: the pane was torn down, the worker slot released, \
         and the cube workspace lease force-released. The work item is reset for redispatch.\n\n\
         {advice}"
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        body_markdown: body,
        kind: DRIVER_START_ATTENTION_KIND.to_owned(),
        title,
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

/// Raise the per-execution attention item for a slot whose liveness has
/// stayed [`crate::transcript_liveness::TranscriptLiveness::Undeterminable`]
/// for [`UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD`] consecutive sweep
/// passes.
///
/// Nothing about this outcome reaps the slot — an unreadable answer is not
/// an absent transcript — but several of its causes (a cube workspace
/// already reclaimed, a checkpoint that will not deserialize) do not heal on
/// their own, so without this the slot, its cube lease, and its work item
/// would be held for the life of the run with nothing beyond an `error`-level
/// log line to show for it. Best-effort — a failure here must never affect
/// the (non-)reap decision, which has already been made by the caller.
fn raise_liveness_undeterminable_attention(
    work_db: &WorkDb,
    execution: &WorkExecution,
    slot_id: u8,
    shell_pid: i32,
    reasons: &[String],
    checked: &[String],
) {
    let execution_id = execution.id.as_str();
    let mut body = format!(
        "**Observed (liveness veto, spawn-ack sweep):** for execution `{execution_id}` on slot {slot_id} \
         (shell pid `{shell_pid}`), the liveness veto could not determine whether a transcript exists on \
         disk across {UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD} consecutive sweep passes.\n\n\
         **Could not be established:** {}\n\n",
        reasons.join("; "),
    );
    if !checked.is_empty() {
        body.push_str(&format!("**Also checked:** {}\n\n", checked.join("; ")));
    }
    body.push_str(
        "The engine has NOT reaped this execution — an unreadable answer is not proof the driver never \
         ran, and reaping on one risks killing a real worker. But several causes of this state do not \
         resolve on their own (a cube workspace that no longer canonicalizes, a stored checkpoint that \
         will not deserialize), so the slot, its cube workspace lease, and its work item may be held \
         indefinitely without operator attention. Investigate the reasons above; if the execution is \
         genuinely dead, it can be reaped manually.",
    );
    if let Err(err) = work_db.create_attention_item(CreateAttentionItemInput {
        body_markdown: body,
        kind: LIVENESS_UNDETERMINABLE_ATTENTION_KIND.to_owned(),
        title: format!("Worker liveness could not be determined for slot {slot_id}"),
        execution_id: Some(execution_id.to_owned()),
        resolved_at: None,
        status: None,
        work_item_id: None,
    }) {
        tracing::warn!(
            execution_id,
            ?err,
            "liveness-undeterminable: failed to raise attention item",
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
#[path = "spawn_ack_sweep_ingress_tests.rs"]
mod ingress_tests;

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
    use crate::semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition};
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
            "grok-4.6",
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
            slot_never_started(&live_states, &pristine),
            "no pid, no hook event, still Spawning is the never-started shape",
        );

        let with_pid = LiveWorkerState {
            shell_pid: 4242,
            ..pristine.clone()
        };
        assert!(
            !slot_never_started(&live_states, &with_pid),
            "a reported shell pid is proof of life"
        );

        let with_event = LiveWorkerState {
            last_event_at: Some("2026-07-31T00:00:00Z".to_owned()),
            ..pristine.clone()
        };
        assert!(
            !slot_never_started(&live_states, &with_event),
            "a hook event is proof of life"
        );

        let progressed = LiveWorkerState {
            activity: WorkerActivity::Working,
            ..pristine.clone()
        };
        assert!(
            !slot_never_started(&live_states, &progressed),
            "activity past Spawning is proof of life",
        );
    }

    /// A driver-start timeout must not assert "the binary never started"
    /// when the run's file ingress says otherwise. In the 2026-09-13 breaker
    /// incident five Codex workers were reaped with exactly that text while
    /// their rollouts — created seconds after the ingress's old give-up
    /// point — showed hundreds of thousands of tokens of work.
    #[test]
    fn driver_start_timeout_narrates_the_file_ingress_reading() {
        let ingress = FileIngressState {
            summary: "the engine's file ingress never attached to a rollout: discovery was overdue at \
                      121s (0 rollout-shaped file(s) rejected as not this run's) and still looking, so \
                      the driver may have started and run unobserved"
                .to_owned(),
            details: serde_json::json!({ "state": "armed" }),
        };
        let (reason, audit, stage) = reap_narrative(
            &ReapCause::DriverStartTimeout {
                grace_secs: 300,
                silent_secs: 302,
                activity: "spawning",
                file_ingress: Some(ingress),
            },
            "exec-1",
        );
        assert_eq!(stage, Stage::DriverStartTimeout);
        assert!(reason.starts_with("driver-start-timeout:"), "{reason}");
        assert!(
            !reason.contains("never started") && !audit.contains("never ran"),
            "must not assert the binary never ran when the ingress says it may have; got: {reason} / {audit}",
        );
        assert!(
            reason.contains("may have started and run unobserved")
                && audit.contains("may have started and run unobserved"),
            "both surfaces carry the ingress reading; got: {reason} / {audit}",
        );

        // With no file ingress at all (a hook-socket driver), the historical
        // reading stands — but as a likelihood, not a fact.
        let (reason, _, _) = reap_narrative(
            &ReapCause::DriverStartTimeout {
                grace_secs: 300,
                silent_secs: 302,
                activity: "spawning",
                file_ingress: None,
            },
            "exec-2",
        );
        assert!(reason.contains("most likely never started"), "{reason}");
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
            "no transcript exists: probe stub",
            0,
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

    /// `unverified_driver_starts` is blind to `shell_pid` by construction, so
    /// pass 2 can reach a candidate that never reported one at all. The
    /// narrative must say so rather than asserting "a pane and shell came
    /// up" for a pid it never observed.
    #[test]
    fn driver_start_timeout_narrative_reflects_whether_a_shell_pid_was_ever_reported() {
        let cause = ReapCause::DriverStartTimeout {
            grace_secs: 300,
            silent_secs: 400,
            activity: "spawning",
        };

        let (reason, audit, _) = reap_narrative(&cause, "exec-1", "no transcript exists: probe stub", 4242);
        assert!(
            reason.contains("a pane and shell came up") && audit.contains("a pane and shell came up"),
            "a reported pid must still be narrated as a pane and shell coming up; got: {reason} / {audit}",
        );

        let (reason, audit, _) = reap_narrative(&cause, "exec-1", "no transcript exists: probe stub", 0);
        assert!(
            !reason.contains("a pane and shell came up") && !audit.contains("a pane and shell came up"),
            "a zero pid must not be narrated as a shell coming up; got: {reason} / {audit}",
        );
        assert!(
            reason.contains("no shell pid was ever reported") && audit.contains("no shell pid was ever reported"),
            "got: {reason} / {audit}",
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
        // `AlwaysSucceedsCube`, not `NoopCube`: the steady-state rescan this
        // coordinator performs after a reap can requeue and redispatch a
        // chore before this sweep pass returns (an extra `spawn_blocking`
        // await point in the liveness probe gives the current-thread runtime
        // more chances to interleave that redispatch's own completion —
        // including a second reap of it — into this same pass), and a
        // redispatched execution's lease is released through the same
        // `cube_client` this call passes, not only through the coordinator's
        // own. See `AlwaysSucceedsCube`'s doc for the exact scenario.
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &AlwaysSucceedsCube,
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
        // See the comment in `systemic_spawn_failure_trips_capability_breaker_once`
        // for why this must be `AlwaysSucceedsCube`, not `NoopCube`.
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &AlwaysSucceedsCube,
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
        let live_states = LiveWorkerStateRegistry::new();
        let ctx = SpawnReapCtx::builder()
            .work_db(db.as_ref())
            .live_states(&live_states)
            .coordinator(coordinator.clone())
            .dispatch_events(sink.as_ref())
            .reaper(reaper.as_ref())
            .spawn_health(&spawn_health)
            .cube_client(&NoopCube)
            .build();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let reason = "ghostty_surface_new returned NULL (no active display)";
        let reaped = reap_never_started_spawn(&ctx, &execution, 1, 0, ReapCause::AppNack { reason }, now).await;

        assert_eq!(
            reaped,
            ReapOutcome::Reaped,
            "app NACK must reap the never-started spawn"
        );
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

    // ─── the liveness veto (2026-09-13) ──────────────────────────────────────

    /// A `session_meta` first line in the shape Codex writes, with `cwd`
    /// pointing wherever the test wants correlation to land.
    fn session_meta_line(session_id: &str, cwd: &std::path::Path) -> String {
        format!(
            "{}\n",
            serde_json::json!({
                "timestamp": "2026-09-13T21:41:42.000Z",
                "type": "session_meta",
                "payload": {
                    "id": session_id,
                    "timestamp": "2026-09-13T21:41:42.000Z",
                    "cwd": cwd.display().to_string(),
                    "originator": "codex_cli_rs",
                    "cli_version": "0.0.0-test",
                }
            })
        )
    }

    /// The durable shape a Codex spawn leaves behind: a run row and an
    /// `Armed` ingress checkpoint pointing at `root` with the given
    /// baseline. Returns the workspace the ingress correlates against.
    fn arm_file_ingress(
        db: &WorkDb,
        execution_id: &str,
        root: &std::path::Path,
        workspace: &std::path::Path,
        baseline: Vec<std::path::PathBuf>,
    ) {
        use crate::agent_jsonl_progress::{IngressCheckpoint, IngressCheckpointStore};
        let checkpoint = IngressCheckpoint::Armed {
            ingress: crate::driver::AgentJsonlFileIngress {
                directory: root.to_path_buf(),
                filename_prefix: "rollout-".to_owned(),
                filename_suffix: ".jsonl".to_owned(),
                workspace_path: workspace.to_path_buf(),
            },
            baseline,
        };
        db.store_ingress_checkpoint(execution_id, &checkpoint)
            .expect("the run row exists, so the checkpoint can be stored");
    }

    /// The incident, reproduced: a pane with a live shell, no driver
    /// signal for longer than the window, and a rollout on disk that the
    /// progress ingress never attached. The reap must be refused, the
    /// transcript recorded as the run's driver-start proof, and nothing
    /// torn down. Run with a rollout that correlates and with one discovery
    /// would reject (the 43-second-margin case): the driver wrote both, so
    /// both are proof of life.
    #[tokio::test]
    async fn a_transcript_on_disk_vetoes_the_driver_start_reap() {
        for correlates in [true, false] {
            let (_dir, db) = open_db();
            let product_id = create_product(&db);
            let work_item_id = create_active_chore(&db, &product_id, "test chore");
            let db = Arc::new(db);

            let execution_id = create_spawned_execution(&db, &work_item_id, 92697);
            let temp = tempfile::TempDir::new().unwrap();
            let root = temp.path().join("sessions");
            let workspace = temp.path().join("workspace");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&workspace).unwrap();
            arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
            let cwd = if correlates {
                workspace.clone()
            } else {
                temp.path().to_path_buf()
            };
            let rollout = root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl");
            std::fs::write(&rollout, session_meta_line("sess-1", &cwd)).unwrap();

            let live_states = Arc::new(LiveWorkerStateRegistry::new());
            register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 92697, false);
            let coordinator = make_coordinator(db.clone(), 1);
            coordinator.worker_pool().claim_worker(&execution_id, None).await;

            let cube = RecordingCube::default();
            let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

            assert_eq!(
                outcome.driver_start_reaped, 0,
                "correlates={correlates}: a worker with a transcript on disk must not be reaped",
            );
            assert_eq!(outcome.vetoed, 1, "correlates={correlates}: the veto must be counted");
            assert_ne!(
                db.get_execution(&execution_id).unwrap().status,
                ExecutionStatus::Orphaned,
                "correlates={correlates}: the execution must not be orphaned",
            );
            assert!(
                coordinator
                    .worker_pool()
                    .claimed_execution_ids()
                    .await
                    .contains(&execution_id),
                "correlates={correlates}: the slot must stay claimed",
            );
            assert!(
                cube.released_lease_ids().is_empty(),
                "correlates={correlates}: the cube lease must not be released",
            );
            assert!(
                sink.events().await.is_empty(),
                "correlates={correlates}: no reap event may be emitted",
            );
            assert!(
                db.list_attention_items(&execution_id).unwrap().is_empty(),
                "correlates={correlates}: no attention item may be raised",
            );
            assert!(
                live_states.driver_signal_at(1).is_some(),
                "correlates={correlates}: the transcript must be recorded as driver-start proof",
            );

            // The proof is permanent: a second pass finds nothing to examine.
            let (again, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
            assert_eq!(
                again.driver_start_reaped + again.vetoed + again.liveness_undeterminable,
                0
            );
        }
    }

    /// Liveness that cannot be established is not absence. A checkpoint
    /// whose root cannot be verified must leave the slot alone and say so,
    /// rather than reaping on an unreadable answer.
    #[tokio::test]
    async fn undeterminable_liveness_does_not_reap() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
        let temp = tempfile::TempDir::new().unwrap();
        let missing_root = temp.path().join("never-created");
        arm_file_ingress(&db, &execution_id, &missing_root, temp.path(), Vec::new());

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 4242, false);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(outcome.driver_start_reaped, 0);
        assert_eq!(outcome.liveness_undeterminable, 1);
        assert_ne!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );
        assert!(sink.events().await.is_empty());
        assert!(
            live_states.driver_signal_at(1).is_none(),
            "an undeterminable answer is not proof either way",
        );

        // A liveness answer that stays undeterminable pass after pass must
        // not be held silently forever: once the consecutive count crosses
        // the threshold, a distinct attention item is raised exactly once.
        for pass in 2..UNDETERMINABLE_LIVENESS_ATTENTION_THRESHOLD {
            let (outcome, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
            assert_eq!(outcome.liveness_undeterminable, 1, "pass {pass}");
            assert!(
                db.list_attention_items(&execution_id).unwrap().is_empty(),
                "pass {pass}: no attention item before the threshold is crossed",
            );
        }
        let (outcome, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
        assert_eq!(outcome.liveness_undeterminable, 1, "the threshold-crossing pass");
        let attentions = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(
            attentions.len(),
            1,
            "exactly one attention item once the threshold is crossed"
        );
        assert_eq!(attentions[0].kind, LIVENESS_UNDETERMINABLE_ATTENTION_KIND);

        // Further passes must not raise a second one.
        let (outcome, _) = run_pass(&db, &live_states, &coordinator, &cube).await;
        assert_eq!(outcome.liveness_undeterminable, 1);
        assert_eq!(
            db.list_attention_items(&execution_id).unwrap().len(),
            1,
            "the attention item must not be raised again on subsequent passes",
        );
    }

    /// A confirmed absence still reaps — the breaker must keep firing on
    /// genuinely dead spawns — and the record says what was checked and
    /// which failure class fired, not that the driver "never started".
    #[tokio::test]
    async fn absent_transcript_reaps_and_the_record_states_what_was_checked() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 4242);
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        // A rollout from before the spawn is baselined away and must not
        // count as this run's transcript.
        let stale = root.join("rollout-old-sess-0.jsonl");
        std::fs::write(&stale, session_meta_line("sess-0", temp.path())).unwrap();
        let stale = std::fs::canonicalize(&stale).unwrap();
        arm_file_ingress(&db, &execution_id, &root, temp.path(), vec![stale]);

        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        register_slot_with_live_shell(&live_states, 1, &execution_id, &work_item_id, 4242, false);
        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let spawn_health = SpawnHealthTracker::new();
        let cube = RecordingCube::default();
        let outcome = run_one_pass(
            db.as_ref(),
            &live_states,
            coordinator.clone(),
            sink.as_ref(),
            reaper.as_ref(),
            &spawn_health,
            &cube,
            SPAWN_ACK_GRACE_SECS,
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.driver_start_reaped, 1, "a confirmed absence must still reap");
        assert_eq!(cube.released_lease_ids(), vec!["lease-1".to_owned()]);
        assert_eq!(outcome.vetoed, 0);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned
        );

        let attentions = db.list_attention_items(&execution_id).unwrap();
        assert_eq!(attentions.len(), 1);
        let body = &attentions[0].body_markdown;
        assert!(
            body.contains("no rollout file newer than the pre-spawn baseline"),
            "the attention body must state the liveness probe's finding; got: {body}",
        );
        assert!(
            body.contains("1 pre-existing file(s) excluded"),
            "the probe must report the baselined file it ignored; got: {body}",
        );
        assert!(
            !body.contains("The driver binary never started") && !body.contains("driver binary never ran"),
            "the body must not assert the inference that the driver never started; got: {body}",
        );
        assert!(
            body.contains("Either the driver never started, or it started and its signal never reached"),
            "the body must name both explanations for the missing signal; got: {body}",
        );
        assert!(
            attentions[0].title.contains("no driver signal was observed"),
            "got: {}",
            attentions[0].title,
        );

        let events = sink.events().await;
        let reap = events
            .iter()
            .find(|e| e.stage == "driver_start_timeout")
            .expect("the reap event");
        assert_eq!(
            reap.details["failure_class"],
            serde_json::json!("shell_without_driver_signal")
        );
        assert!(
            reap.details["liveness_probe"]
                .as_str()
                .unwrap()
                .contains("no transcript exists"),
            "got: {}",
            reap.details["liveness_probe"],
        );

        let evidence = spawn_health.evidence_in_window(boss_engine_utils::epoch_time::now_epoch_secs());
        assert_eq!(evidence.len(), 1);
        assert_eq!(
            evidence[0].class,
            crate::spawn_health::SpawnFailureClass::ShellWithoutDriverSignal,
            "pass 2 must feed the breaker as its own failure class",
        );
        assert_eq!(evidence[0].cause, "driver_start_timeout");
        assert!(
            evidence[0].observed.contains("liveness probe:"),
            "got: {}",
            evidence[0].observed
        );
    }

    /// The liveness veto does NOT protect the app-reported causes: an
    /// `AppNack` is the app itself positively reporting the pane failed to
    /// spawn, which a transcript's mere existence does not contradict.
    /// Before this fix the veto applied uniformly to every cause, leaving a
    /// `pid<=0`/`Spawning` slot behind that no other sweep could reclaim
    /// (see the module doc and `ReapCause::vetoable`'s doc) — the reap must
    /// proceed here, and the transcript must NOT be recorded as a permanent
    /// driver signal.
    #[tokio::test]
    async fn app_nack_is_not_vetoed_by_a_transcript_on_disk() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 0);
        let execution = db.get_execution(&execution_id).unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
        std::fs::write(
            root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
            session_meta_line("sess-1", &workspace),
        )
        .unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let spawn_health = SpawnHealthTracker::new();
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let live_states = LiveWorkerStateRegistry::new();
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
        let cube = RecordingCube::default();
        let ctx = SpawnReapCtx::builder()
            .work_db(db.as_ref())
            .live_states(&live_states)
            .coordinator(coordinator.clone())
            .dispatch_events(sink.as_ref())
            .reaper(reaper.as_ref())
            .spawn_health(&spawn_health)
            .cube_client(&cube)
            .build();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let outcome =
            reap_never_started_spawn(&ctx, &execution, 1, 0, ReapCause::AppNack { reason: "late" }, now).await;

        assert_eq!(
            outcome,
            ReapOutcome::Reaped,
            "an app-reported NACK must reap despite a transcript on disk"
        );
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned,
        );
        assert_eq!(reaper.reaped().len(), 1, "the pane must still be torn down");
        assert_eq!(sink.events().await.len(), 1);
        assert!(
            live_states.driver_signal_at(1).is_none(),
            "an app-reported cause must never record a permanent driver signal from the veto probe",
        );
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "the slot must be released — reachable by redispatch, not stuck the way a vetoed slot is",
        );
    }

    /// Same as above for the other app-reported cause: a pane the app
    /// reports as dead-before-start must be reaped even with a transcript
    /// on disk.
    #[tokio::test]
    async fn pane_died_before_start_is_not_vetoed_by_a_transcript_on_disk() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 0);
        let execution = db.get_execution(&execution_id).unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("sessions");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        arm_file_ingress(&db, &execution_id, &root, &workspace, Vec::new());
        std::fs::write(
            root.join("rollout-2026-09-13T21-41-42-sess-1.jsonl"),
            session_meta_line("sess-1", &workspace),
        )
        .unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let spawn_health = SpawnHealthTracker::new();
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let live_states = LiveWorkerStateRegistry::new();
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
        let cube = RecordingCube::default();
        let ctx = SpawnReapCtx::builder()
            .work_db(db.as_ref())
            .live_states(&live_states)
            .coordinator(coordinator.clone())
            .dispatch_events(sink.as_ref())
            .reaper(reaper.as_ref())
            .spawn_health(&spawn_health)
            .cube_client(&cube)
            .build();
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::PaneDiedBeforeStart {
                detail: "surface failed to attach",
            },
            now,
        )
        .await;

        assert_eq!(outcome, ReapOutcome::Reaped);
        assert_eq!(
            db.get_execution(&execution_id).unwrap().status,
            ExecutionStatus::Orphaned,
        );
        assert!(live_states.driver_signal_at(1).is_none());
    }

    /// The recorded-transcript-path source must not vouch for a run that
    /// merely reused a run row an earlier incarnation already stamped a
    /// transcript path onto: a file last written before this execution's
    /// `started_at` must not veto a vetoable cause.
    #[tokio::test]
    async fn a_transcript_path_recorded_before_this_spawn_does_not_veto() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 0);
        let temp = tempfile::TempDir::new().unwrap();
        let transcript = temp.path().join("earlier-incarnation.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        db.set_run_transcript_path_if_unset(&execution_id, transcript.to_str().unwrap())
            .unwrap();

        // Force `started_at` to AFTER the transcript file's mtime, simulating
        // a later incarnation of the same execution row reusing a run whose
        // `transcript_path` a prior, unrelated spawn already recorded.
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        db.force_started_at_for_test(&execution_id, now + 1000).unwrap();
        let execution = db.get_execution(&execution_id).unwrap();

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;
        let spawn_health = SpawnHealthTracker::new();
        let reaper = Arc::new(RecordingReaper::new(coordinator.clone()));
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let live_states = LiveWorkerStateRegistry::new();
        register_slot_zero_pid(&live_states, 1, &execution_id, &work_item_id);
        let cube = RecordingCube::default();
        let ctx = SpawnReapCtx::builder()
            .work_db(db.as_ref())
            .live_states(&live_states)
            .coordinator(coordinator.clone())
            .dispatch_events(sink.as_ref())
            .reaper(reaper.as_ref())
            .spawn_health(&spawn_health)
            .cube_client(&cube)
            .build();
        let outcome = reap_never_started_spawn(
            &ctx,
            &execution,
            1,
            0,
            ReapCause::SpawnAckTimeout { grace_secs: 60 },
            now + 2000,
        )
        .await;

        assert_eq!(
            outcome,
            ReapOutcome::Reaped,
            "a transcript path predating this run's spawn must not veto the reap"
        );
        assert!(
            live_states.driver_signal_at(1).is_none(),
            "a stale recorded transcript path must not be recorded as this run's driver signal",
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

    /// Re-adoption then sweep: a durable driver signal from before the
    /// engine restart must leave the worker entirely alone.
    ///
    /// `readopt_live_worker` restores the durable semantic-progress
    /// checkpoint after reconstructing the slot. That checkpoint came from
    /// a driver-originated event before restart, so it proves this run is
    /// not a never-started driver even though this registration itself was
    /// triggered by a shell-pid probe.
    #[tokio::test]
    async fn a_readopted_worker_with_durable_driver_proof_is_not_reaped() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_spawned_execution(&db, &work_item_id, 92697);
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        // Exactly what `readopt_live_worker` does when its durable-pid probe
        // cannot produce a positive pid.
        live_states.register_readoption(
            1,
            execution_id.as_str(),
            "grok-4.6",
            0,
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
        live_states.seed_semantic_progress(
            1,
            &SemanticProgressCheckpoint {
                progress_at: "2026-09-02T12:00:00Z".to_owned(),
                tool_condition: SemanticToolCondition::Unknown,
            },
        );
        assert!(
            live_states.driver_signal_at(1).is_some(),
            "a durable checkpoint restores proof that the driver signalled before restart",
        );

        let coordinator = make_coordinator(db.clone(), 1);
        coordinator.worker_pool().claim_worker(&execution_id, None).await;

        let cube = RecordingCube::default();
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.driver_start_reaped, 0,
            "a re-adopted worker with durable driver proof must not be reaped",
        );
        assert_eq!(outcome.reaped, 0);
        assert_eq!(
            outcome.skipped.readopted, 1,
            "the readopted skip must be counted, not silently dropped from the accounting",
        );
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
