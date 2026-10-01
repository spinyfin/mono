//! Periodic reconciler that detects and reaps worker slots whose spawn
//! never produced a driver-originated signal — the "false-live"
//! failure class from the 2026-07-30 incident (a pane hosting only a
//! login shell) and the 2026-09-13 liveness-veto follow-on.
//!
//! Local workers are created in tmux. [`crate::spawn_flow::start_tmux_worker`]
//! treats a missing or zero pane pid as a spawn failure, so a `Spawning`
//! slot with `shell_pid == 0` is a tmux-invariant violation, not a
//! timeout: this sweep logs it and does not reap on that evidence.
//! Driver-start verification (below) still covers a slot that reported
//! a pid but never produced a driver signal.
//!
//! ## The 2026-07-30 incident: a pane that DID have a shell, and no driver
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
//! as proof of a working worker. This module reaps on driver-start
//! silence, and logs a tmux-invariant violation if a local slot is
//! `Spawning` with no pane pid at all.
//!
//! ## Algorithm
//!
//! ### Tmux-invariant check (pid-less `Spawning`)
//!
//! Snapshot [`LiveWorkerStateRegistry`]. A `Spawning` slot with
//! `shell_pid == 0`, no driver signal, and not a re-adoption is illegal
//! under tmux-only spawn: [`crate::spawn_flow`] refuses to register a
//! local worker without `#{pane_pid}`. Log it; do not reap on that
//! evidence. Remote virtual slots record a driver signal before they
//! register; readopted slots are skipped.
//!
//! ### Driver-start timeout (the 2026-07-30 class)
//!
//! [`LiveWorkerStateRegistry::unverified_driver_starts`] returns every
//! live slot past [`crate::live_worker_state::DRIVER_START_GRACE_SECS`] with no driver-originated
//! signal. That query reads only `driver_signal_at` and `spawned_at`, so
//! it is blind to `shell_pid`, to `activity`, and to the driver's
//! capability set: it covers grok exactly as it covers claude, and it
//! sees a slot `mark_stalled_spawns` has promoted out of `Spawning` just
//! as well as one still sitting in it.
//!
//! Candidates funnel into [`reap_never_started_spawn`], which first
//! consults the liveness veto (below), then marks the
//! execution `orphaned`, appends an `[engine-reconcile]` audit line,
//! reaps the pane through the same `release_worker_pane` teardown
//! `bossctl agents stop` uses, releases the pool slot, force-releases the
//! cube workspace lease, emits a dispatch event, and kicks the
//! coordinator so the orphan sweep redispatches the never-started work.
//! Driver-start reaps additionally raise an attention item (see below).
//!
//! ## False-positive guards
//!
//! [`crate::live_worker_state::DRIVER_START_GRACE_SECS`] (300s) is an order of
//! magnitude above real driver startup — a healthy driver's `SessionStart`
//! hook fires within seconds of exec. See that constant's doc for why
//! claude's folder-trust dialog, the one historically legitimate
//! multi-minute pre-hook wait, cannot produce a false positive here.
//!
//! A slot that produces a single driver signal before its window elapses
//! is left alone permanently: `driver_signal_at` is
//! first-write-wins and is never cleared for the life of the run.
//!
//! ## Why driver-start raises an attention item
//!
//! Reaps feed [`crate::spawn_health`] — the reap is shared, so every
//! cause records evidence, records a failure against the work item, and can
//! trip the spawn-capability breaker that pauses dispatch once enough
//! DISTINCT work items fail inside the window. That is deliberate for a
//! driver-start timeout: a driver binary that cannot exec on this host
//! fails identically for every work item routed to it, which is exactly the
//! systemic shape the breaker exists to stop, and the alternative — reaping
//! and redispatching forever without ever pausing — is the churn the breaker
//! was built to end.
//!
//! Visibility is separate. A pane genuinely came up and a live process was
//! left holding a workspace with no driver in it. A single aggregate item
//! cannot name which workspace is still held, and a lone occurrence — the
//! 2026-07-30 incident was one — is below any aggregate threshold and would
//! surface nowhere at all. So a driver-start reap additionally raises its
//! own per-execution item ([`DRIVER_START_ATTENTION_KIND`]) on top of the
//! aggregation, rather than instead of it.
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
//! anything. [`ReapCause::DriverStartTimeout`] is the cause this sweep
//! infers from silence. A transcript for the execution on disk (a rollout
//! newer than the pre-spawn baseline under the run's ingress root, or the
//! run row's recorded transcript path scoped to this incarnation) is
//! driver-originated evidence in its own right: the reap records it as a
//! driver signal
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
//! **The failure class.** Every reap records its
//! [`crate::spawn_health::SpawnFailureClass`] on the breaker evidence, and
//! the pause reason and attention item are composed from the classes
//! actually observed. Driver-start wording — the log line, the orphan
//! reason, the per-execution attention item — states what was observed
//! (a pane and shell came up; no driver-originated signal arrived; what
//! the liveness probe found) rather than the inference "the driver binary
//! never started", which the incident showed can be false.
//!
//! ## Cadence
//!
//! Runs every 60 seconds and fires once immediately on boot (same
//! pattern as [`crate::dead_pid_sweep`] / [`crate::stale_worker_sweep`]).

use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{CreateAttentionItemInput, WorkExecution, WorkerActivity};

use crate::agent_jsonl_progress::{DiscoveryVerdict, IngressCheckpoint, IngressCheckpointStore};
use crate::coordinator::{CubeClient, ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::live_worker_state::{
    DriverSignalKind, DriverStartExpectation, LiveWorkerStateRegistry, NeverStartedReapCommit,
};
use crate::spawn_health::{
    SpawnFailureClass, SpawnHealthTracker, maybe_admit_recovery_probe, trip_spawn_capability_circuit,
};
use crate::transcript_liveness::{TranscriptLiveness, probe_transcript_liveness};
use crate::work::WorkDb;

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

/// Reaps a confirmed never-started spawn's tmux pane and process tree,
/// mirroring [`crate::stale_worker_sweep::StaleWorkerReaper`]. Tearing the
/// session down through `release_worker_pane` is what lets the next
/// dispatch reuse the slot instead of `SlotBusy` rejecting the respawn.
#[async_trait::async_trait]
pub trait SpawnAckReaper: Send + Sync {
    /// Tear down the worker pane (if any) and release resources for
    /// `execution_id`. Idempotent: a slot with no real pane at all is a
    /// no-op.
    async fn reap_worker(&self, execution_id: &str);
}

/// Counts from one pass of the sweep; logged at `info` when a reap
/// occurs.
#[derive(Debug, Default)]
pub struct SpawnAckSweepOutcome {
    /// `Spawning` slots with `shell_pid == 0` that are not a re-adoption
    /// and have no driver signal — illegal under tmux-only spawn, logged
    /// and not reaped on that evidence.
    pub tmux_invariant_pidless: usize,
    /// Reaped by driver-start verification — a pane came up but no driver
    /// ever signalled.
    pub driver_start_reaped: usize,
    /// Reaps refused because a transcript for the execution exists on
    /// disk — see the module doc's liveness veto. Each of these is a
    /// worker that would have been killed alive.
    pub vetoed: usize,
    /// Reaps refused because liveness could not be established. Logged at
    /// error level with the reason; the slot is re-examined next pass.
    pub liveness_undeterminable: usize,
}

impl crate::sweep_loop::SweepOutcome for SpawnAckSweepOutcome {
    fn has_activity(&self) -> bool {
        self.tmux_invariant_pidless > 0
            || self.driver_start_reaped > 0
            || self.vetoed > 0
            || self.liveness_undeterminable > 0
    }

    fn log(&self) {
        tracing::info!(
            tmux_invariant_pidless = self.tmux_invariant_pidless,
            driver_start_reaped = self.driver_start_reaped,
            vetoed = self.vetoed,
            liveness_undeterminable = self.liveness_undeterminable,
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
    driver_start_grace_secs: i64,
) -> SpawnAckSweepOutcome {
    let mut outcome = SpawnAckSweepOutcome::default();
    reconcile_liveness_undeterminable_attention(work_db, live_states);
    let snapshot = live_states.snapshot();

    let now_epoch_secs: i64 = boss_engine_utils::epoch_time::now_epoch_secs();
    let ctx = SpawnReapCtx::builder()
        .work_db(work_db)
        .live_states(live_states)
        .coordinator(Arc::clone(&coordinator))
        .dispatch_events(dispatch_events)
        .reaper(reaper)
        .spawn_health(spawn_health)
        .cube_client(cube_client)
        .build();

    // Tmux-invariant: a local slot cannot be registered with pid 0
    // (`spawn_flow` treats a missing/zero pane pid as a spawn failure).
    // A `Spawning` slot with no pid, no driver signal, and not a
    // re-adoption is therefore a bookkeeping bug, not a timeout.
    for state in &snapshot {
        if state.activity != WorkerActivity::Spawning {
            continue;
        }
        if live_states.driver_start_expectation(state.slot_id) == Some(DriverStartExpectation::Readopted) {
            continue;
        }
        if live_states.driver_signal_at(state.slot_id).is_some() {
            continue;
        }
        if state.shell_pid == 0 {
            tracing::error!(
                execution_id = %state.run_id,
                slot_id = state.slot_id,
                "spawn-ack sweep: tmux-invariant violation: a Spawning slot has no pane pid; \
                 spawn_flow refuses to register a local worker without a tmux pane pid"
            );
            outcome.tmux_invariant_pidless += 1;
        }
    }

    // Driver-start verification: did the DRIVER come up? A pane hosting
    // an idle login shell fails this while satisfying every pane-level
    // check indefinitely.
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
            resolve_liveness_undeterminable_attention(work_db, execution_id);
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
    // anything.
    maybe_admit_recovery_probe(work_db, &coordinator, spawn_health, dispatch_events, now_epoch_secs).await;

    outcome
}

/// Shared references the reap path needs, bundled so
/// [`reap_never_started_spawn`] stays under the argument-count lint.
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
pub(crate) enum ReapCause {
    /// A pane came up — possibly with a live shell pid — and no
    /// driver-originated signal was observed within the window. That is
    /// what was seen; whether the driver never executed or its signal never
    /// reached the engine is what the liveness veto in
    /// [`reap_never_started_spawn`] decides. This cause also raises a
    /// per-execution attention item: nothing else in Boss surfaces it.
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

impl ReapCause {
    /// Which failure shape this cause observed — the breaker composes its
    /// pause reason from these, so the wording an operator reads matches
    /// what each reap actually saw. Consults `shell_pid` rather than mapping
    /// purely on the variant: a pid-less driver-start timeout is `NoShell`.
    pub(crate) fn failure_class(&self, shell_pid: i32) -> SpawnFailureClass {
        match self {
            ReapCause::DriverStartTimeout { .. } => {
                if shell_pid > 0 {
                    SpawnFailureClass::ShellWithoutDriverSignal
                } else {
                    SpawnFailureClass::NoShell
                }
            }
        }
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
fn pane_observation(shell_pid: i32) -> &'static str {
    if shell_pid > 0 {
        "a pane and shell came up"
    } else {
        "a pane was spawned but no shell pid was ever reported"
    }
}

fn reap_narrative(
    cause: &ReapCause,
    execution_id: &str,
    liveness: &TranscriptLiveness,
    shell_pid: i32,
) -> (String, String, Stage) {
    let (reason, audit, stage) = reap_narrative_for_cause(cause, execution_id, shell_pid);
    let summary = liveness.to_string();
    (
        format!("{reason}; liveness probe: {summary}"),
        format!("{audit} Liveness probe before the reap: {summary}."),
        stage,
    )
}

fn reap_narrative_for_cause(cause: &ReapCause, execution_id: &str, shell_pid: i32) -> (String, String, Stage) {
    match &cause {
        ReapCause::DriverStartTimeout {
            grace_secs,
            silent_secs,
            file_ingress,
            ..
        } => {
            // `unverified_driver_starts` is deliberately blind to
            // `shell_pid` (see the module doc), so driver-start can reap a
            // candidate that never reported one (a readopted slot). The
            // narrative asserts "a pane and shell came up" only when a pid
            // was actually reported.
            let pane_observation = pane_observation(shell_pid);
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

/// Reap a slot that produced no driver-originated signal after
/// [`crate::live_worker_state::DRIVER_START_GRACE_SECS`]: mark the execution
/// orphaned, back up any uncommitted work, append an `[engine-reconcile]`
/// audit line, tear the pane down through tmux (`release_worker_pane`),
/// release the pool slot, emit a dispatch event, and feed the spawn-capability
/// circuit breaker — tripping it when too many DISTINCT work items fail in
/// the window. Returns a [`ReapOutcome`]: `Reaped` when all of the above
/// happened; `Vetoed` when the liveness veto refused the reap because a
/// transcript proves the driver ran; `LivenessUndeterminable` when the veto's
/// question could not be answered at all; `Skipped` when the execution was
/// already terminal, or the orphan write failed.
///
/// Called from [`run_one_pass`]'s driver-start check for every
/// [`ReapCause`] it can produce.
///
/// ## The liveness veto
///
/// Before anything is written, the execution's transcript is looked for on
/// disk ([`probe_transcript_liveness`], run via [`tokio::task::spawn_blocking`]
/// so the scan never runs inline on the async runtime). A present transcript
/// is recorded as the run's driver-start signal and the reap returns
/// [`ReapOutcome::Vetoed`]; an undeterminable answer returns
/// [`ReapOutcome::LivenessUndeterminable`] and touches nothing. Only a
/// confirmed absence proceeds to the reap, and the probe's summary is always
/// written into the orphan reason and audit line so the record shows what
/// was checked.
pub(crate) async fn reap_never_started_spawn(
    ctx: &SpawnReapCtx<'_>,
    execution: &WorkExecution,
    slot_id: u8,
    shell_pid: i32,
    cause: ReapCause,
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
            #[cfg(test)]
            probe_hold::wait_if_armed(&execution_id_owned);
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
                    stage = cause.failure_class(shell_pid).as_str(),
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
                    stage = cause.failure_class(shell_pid).as_str(),
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
            resolve_liveness_undeterminable_attention(ctx.work_db, execution_id);
            return ReapOutcome::Vetoed;
        }
        TranscriptLiveness::Undeterminable { reasons, checked } => {
            tracing::error!(
                execution_id,
                work_item_id,
                slot_id,
                shell_pid,
                stage = cause.failure_class(shell_pid).as_str(),
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
                raise_liveness_undeterminable_attention(ctx.work_db, execution, slot_id, shell_pid, reasons, checked);
            }
            return ReapOutcome::LivenessUndeterminable;
        }
        TranscriptLiveness::Absent { .. } => {}
    }

    let shell_pid = match ctx.live_states.confirm_never_started_reap(slot_id, execution_id) {
        NeverStartedReapCommit::Committed { shell_pid } => shell_pid,
        NeverStartedReapCommit::DriverSignalled => {
            tracing::info!(
                execution_id,
                work_item_id,
                slot_id,
                "never-started-spawn reap: a driver-originated signal arrived while the liveness probe \
                 was in flight; abandoning the reap",
            );
            resolve_liveness_undeterminable_attention(ctx.work_db, execution_id);
            return ReapOutcome::Vetoed;
        }
        NeverStartedReapCommit::SlotGone => {
            tracing::info!(
                execution_id,
                work_item_id,
                slot_id,
                "never-started-spawn reap: the live slot no longer belongs to this execution; \
                 abandoning the reap",
            );
            return ReapOutcome::Skipped;
        }
        NeverStartedReapCommit::AlreadyCommitted => {
            tracing::info!(
                execution_id,
                work_item_id,
                slot_id,
                "never-started-spawn reap: a never-started reap is already committed for this \
                 registration; abandoning this pass",
            );
            return ReapOutcome::Skipped;
        }
    };

    let liveness_summary = liveness.to_string();

    let (orphan_reason, audit_note, stage) = reap_narrative(&cause, execution_id, &liveness, shell_pid);

    let orphan_result = {
        #[cfg(test)]
        {
            if let Some(err) = probe_hold::take_orphan_write_failure(execution_id) {
                Err(err)
            } else {
                ctx.work_db.mark_execution_orphaned(execution_id, &orphan_reason)
            }
        }
        #[cfg(not(test))]
        {
            ctx.work_db.mark_execution_orphaned(execution_id, &orphan_reason)
        }
    };
    if let Err(err) = orphan_result {
        tracing::warn!(
            execution_id,
            ?err,
            "reap-never-started-spawn: failed to mark execution orphaned; skipping reap",
        );
        ctx.live_states.release_never_started_reap(slot_id, execution_id);
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
        crate::driver_teardown::TeardownReason::DriverStartTimeout,
    )
    .await;

    // Export the dispatch-created reference. An empty run yields no patch;
    // a missing or unreadable reference raises recovery attention.
    let recovery_patch = crate::execution_bookmark_recovery::backup_dead_execution(ctx.work_db, execution).await;

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

    // Tear down the tmux pane BEFORE the pool slot is released, mirroring
    // the stale-worker sweep's ordering — otherwise a redispatch to the
    // same slot could hit `SlotBusy` if tmux is still holding a session
    // for the slot.
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
                DriverStartEvidence {
                    file_ingress: file_ingress.as_ref(),
                    liveness: &liveness_summary,
                },
            );
        }
    }
    details["failure_class"] = serde_json::json!(cause.failure_class(shell_pid).as_str());
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
    // Driver-start timeouts feed it: a driver binary
    // that cannot exec on this host fails the same way for every work item
    // routed to it, so it belongs in the aggregate. The per-execution
    // attention item above is additional to this, not a
    // replacement for it — see the module doc.
    ctx.spawn_health.record_evidence(
        crate::spawn_health::SpawnFailureEvidence::builder()
            .execution_id(execution_id)
            .work_item_id(work_item_id)
            .slot_id(slot_id.to_string())
            .shell_pid(shell_pid)
            .epoch_secs(now_epoch_secs)
            .class(cause.failure_class(shell_pid))
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

    resolve_liveness_undeterminable_attention(ctx.work_db, execution_id);

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
struct DriverStartEvidence<'a> {
    file_ingress: Option<&'a FileIngressState>,
    liveness: &'a str,
}

fn raise_driver_start_attention(
    work_db: &WorkDb,
    execution: &WorkExecution,
    slot_id: u8,
    shell_pid: i32,
    grace_secs: i64,
    silent_secs: i64,
    evidence: DriverStartEvidence<'_>,
) {
    let DriverStartEvidence { file_ingress, liveness } = evidence;
    let execution_id = execution.id.as_str();
    let reading = driver_start_reading(file_ingress);
    let pid_note = if shell_pid > 0 && file_ingress.is_some() {
        format!("The pane reported shell pid `{shell_pid}`; this does not establish whether the driver ran.")
    } else if shell_pid > 0 {
        format!(
            "The pane reported shell pid `{shell_pid}`, which is why every pane-level check treated \
             this slot as healthy — it identifies the login shell, not the driver inside it. It does \
             not prove the driver started."
        )
    } else {
        "No shell pid was ever reported for this pane.".to_owned()
    };
    let advice = if file_ingress.is_some() {
        "Inspect the file-ingress checkpoint and rollout diagnostics to determine why no driver signal was observed."
    } else {
        "If this repeats for the same driver, the spawn command is most likely not reaching the driver binary at all — check how the command is delivered to the pane."
    };
    let title = format!("Worker spawned on slot {slot_id} but no driver signal was observed");
    let pane_observation = pane_observation(shell_pid);
    let body = format!(
        "**Observed (driver-start timeout):** {pane_observation} for execution \
         `{execution_id}` on slot {slot_id}, but no driver-originated signal — no hook \
         event, no `transcript_path`, no progress-ingress event — was observed within {grace_secs}s \
         (silent for {silent_secs}s).\n\n\
         **Checked before reaping:** {liveness}.\n\n\
         **File ingress:** {reading}.\n\n\
         {pid_note}\n\n\
         The engine has reaped the execution: the pane was torn down, the worker slot released, \
         and the cube workspace lease force-released. The work item is reset for redispatch.\n\n\
         Either the driver never started, or it started and its signal never reached the engine. \
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

/// Clear open `worker_liveness_undeterminable` items whose execution has
/// live driver proof or a durable terminal status, even when the live slot
/// is gone or the execution is no longer a reap candidate.
fn reconcile_liveness_undeterminable_attention(work_db: &WorkDb, live_states: &LiveWorkerStateRegistry) {
    let items = match work_db.list_open_attention_items_of_kind(LIVENESS_UNDETERMINABLE_ATTENTION_KIND) {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(
                ?err,
                "liveness-undeterminable: failed to list open items for reconciliation",
            );
            return;
        }
    };
    for item in items {
        let Some(execution_id) = item.execution_id.as_deref() else {
            continue;
        };
        let terminal = work_db
            .get_execution(execution_id)
            .ok()
            .is_some_and(|execution| execution.status.is_terminal());
        let driver_proof = live_states
            .snapshot()
            .iter()
            .any(|state| state.run_id == execution_id && live_states.driver_signal_at(state.slot_id).is_some());
        if terminal || driver_proof {
            resolve_liveness_undeterminable_attention(work_db, execution_id);
        }
    }
}

/// Resolve any open `worker_liveness_undeterminable` item for this execution.
/// Best-effort: a failure here must never affect the reap/veto decision.
fn resolve_liveness_undeterminable_attention(work_db: &WorkDb, execution_id: &str) {
    if let Err(err) = work_db.resolve_attention_kind_for_execution(execution_id, LIVENESS_UNDETERMINABLE_ATTENTION_KIND)
    {
        tracing::warn!(
            execution_id,
            ?err,
            "liveness-undeterminable: failed to resolve attention item",
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
#[path = "spawn_ack_sweep_probe_hold.rs"]
pub(crate) mod probe_hold;

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
            &TranscriptLiveness::Absent { checked: vec![] },
            4242,
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
            &TranscriptLiveness::Absent { checked: vec![] },
            4242,
        );
        assert!(reason.contains("most likely never started"), "{reason}");
    }

    /// `unverified_driver_starts` is blind to `shell_pid` by construction, so
    /// the driver-start check can reach a candidate that never reported one at all. The
    /// narrative must say so rather than asserting "a pane and shell came
    /// up" for a pid it never observed.
    #[test]
    fn driver_start_timeout_narrative_reflects_whether_a_shell_pid_was_ever_reported() {
        let cause = ReapCause::DriverStartTimeout {
            grace_secs: 300,
            silent_secs: 400,
            file_ingress: None,
            activity: "spawning",
        };

        let absent = TranscriptLiveness::Absent {
            checked: vec!["probe stub".to_owned()],
        };
        let (reason, audit, _) = reap_narrative(&cause, "exec-1", &absent, 4242);
        assert!(
            reason.contains("a pane and shell came up") && audit.contains("a pane and shell came up"),
            "a reported pid must still be narrated as a pane and shell coming up; got: {reason} / {audit}",
        );

        let (reason, audit, _) = reap_narrative(&cause, "exec-1", &absent, 0);
        assert!(
            !reason.contains("a pane and shell came up") && !audit.contains("a pane and shell came up"),
            "a zero pid must not be narrated as a shell coming up; got: {reason} / {audit}",
        );
        assert!(
            reason.contains("no shell pid was ever reported") && audit.contains("no shell pid was ever reported"),
            "got: {reason} / {audit}",
        );
    }

    /// A `Spawning` slot with `shell_pid == 0` is a tmux-invariant
    /// violation (`spawn_flow` refuses to register a local worker without
    /// `#{pane_pid}`). It is logged, not reaped as a spawn-ack timeout.
    #[tokio::test]
    async fn pidless_spawning_slot_is_a_tmux_invariant_violation_not_a_reap() {
        let (_dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let db = Arc::new(db);

        let execution_id = create_old_execution(&db, &work_item_id);
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
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(
            outcome.tmux_invariant_pidless, 1,
            "a pid-less Spawning slot is a tmux-invariant violation"
        );
        assert_eq!(
            outcome.driver_start_reaped, 0,
            "fresh spawned_at is inside the driver-start window"
        );
        assert!(reaper.reaped().is_empty());
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
        assert!(
            coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&execution_id),
            "pool slot must stay claimed — this is not a reap"
        );
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
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(
            outcome.driver_start_reaped, 0,
            "a fresh pid-bearing slot is inside the driver-start window"
        );
        assert_eq!(outcome.tmux_invariant_pidless, 0);
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
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(
            outcome.driver_start_reaped, 0,
            "a slot with any hook event must not be reaped"
        );
        assert_eq!(outcome.tmux_invariant_pidless, 0);
        assert!(sink.events().await.is_empty());
        assert_eq!(db.get_execution(&execution_id).unwrap().status, ExecutionStatus::Ready);
    }

    /// A slot already past `Spawning` (e.g. `Working`) is not a
    /// tmux-invariant pid-less candidate. A fresh `spawned_at` also keeps
    /// it out of driver-start verification.
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
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.driver_start_reaped, 0);
        assert_eq!(outcome.tmux_invariant_pidless, 0);
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

        // Four distinct chores, each with a pane that came up and no driver.
        let mut execution_ids = Vec::new();
        let live_states = Arc::new(LiveWorkerStateRegistry::new());
        for slot in 1u8..=4 {
            let work_item_id = create_active_chore(&db, &product_id, &format!("chore {slot}"));
            let execution_id = create_old_execution(&db, &work_item_id);
            register_slot_with_live_shell(&live_states, slot, &execution_id, &work_item_id, 4242, false);
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
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.driver_start_reaped, 4, "every silent driver-start is reaped");
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
            register_slot_with_live_shell(&live_states, slot, &execution_id, &work_item_id, 4242, false);
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
            DRIVER_START_GRACE_SECS,
        )
        .await;

        assert_eq!(outcome.driver_start_reaped, 3, "every silent driver-start is reaped");
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
        let _bookmark_store = crate::test_support::seed_empty_execution_bookmark(&db, &execution_id).await;
        let (outcome, sink) = run_pass(&db, &live_states, &coordinator, &cube).await;

        assert_eq!(
            outcome.driver_start_reaped, 1,
            "a pane with a live shell pid and no driver signal must be reaped",
        );
        assert_eq!(
            outcome.tmux_invariant_pidless, 0,
            "a reported pid is not a tmux-invariant pid-less slot",
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
    /// be reached. A `Spawning`-only filter would miss it; if the driver-start check shared
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

    #[path = "spawn_ack_sweep_liveness_tests.rs"]
    mod liveness_tests;
}
