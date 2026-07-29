//! Engine-owned reconciler that auto-recovers workers wedged by a
//! *transient* Claude API error.
//!
//! ## The failure this closes
//!
//! Boss launches each worker as an **interactive** `claude` session in
//! a libghostty pane (`runner.rs`: `claude … "$(cat initial-prompt.txt)"`
//! with no `--print`). When claude exhausts its own internal retries on
//! a transient API error — "API Error: The socket connection was closed
//! unexpectedly", `overloaded_error`, a 5xx, a 429, a request timeout —
//! it prints the error, ends the turn, and returns to its REPL. The
//! events socket reports the turn-ending `Stop` as `Idle`, so the
//! worker *looks done* while actually being wedged mid-chore. The
//! dead-PID sweep can't see it (the process is alive); the completion
//! path can't see it (no PR, no clean finish). Before this module a
//! human had to notice and restart the run (the Yar/T678 incident).
//!
//! ## Design: nudge-first, orphan+respawn as fallback
//!
//! On a transient API error the worker's `claude` process stays alive
//! at its REPL — it printed the error, ended the turn, and returned to
//! its prompt. For this alive-but-idle case the cheap first recovery is
//! a runtime nudge ("your previous turn ended on a transient API error;
//! please retry the last step") injected via the same channel as
//! `bossctl agents send`. Full orphan+respawn (spawn a fresh `claude`
//! process on the same workspace) is reserved for cases where the
//! worker is actually dead, the nudge did not clear the error by the
//! next sweep, or the error is permanent.
//!
//! Each pass, for every non-actively-working worker slot whose backing
//! execution is old enough ([`RECOVERY_GRACE_SECS`]):
//!
//!   1. Read the worker's transcript tail — the authoritative signal.
//!      [`crate::transient_error::extract_worker_error`] returns the
//!      halting API-error text **only if it is the last meaningful
//!      entry** (if the worker did any work after the error it
//!      recovered on its own and we leave it alone). We never trust the
//!      `Idle` hook alone — it can't distinguish "finished cleanly" from
//!      "wedged on an error."
//!   2. Classify the error through the resolved driver's
//!      [`crate::driver::AgentDriver::classify_error`] and apply
//!      the bounded-retry policy
//!      ([`crate::transient_error::RecoveryPolicy`]).
//!   3. **Nudge** (transient, under cap, worker alive and idle, not
//!      already nudged this session): send a runtime message into the
//!      existing `claude` REPL asking it to retry. If the nudge fails
//!      (send error, unknown slot, etc.) fall through to orphan+respawn.
//!      On the next sweep if the error is still the last entry,
//!      increment the attempt counter and proceed to orphan+respawn.
//!   4. **Resume** (transient, under the cap, not nudgeable): orphan the
//!      dead execution and insert a fresh `ready` one that prefers the
//!      same cube workspace with `allow_dirty = true` (so `--prefer
//!      --allow-dirty` re-leases the exact workspace *without* cube
//!      resetting it, and the in-progress jj branch is not lost),
//!      carrying an incremented `transient_failure_count` and a
//!      `dispatch_not_before` backoff. The runner's existing
//!      startup-recovery prompt then directs the new worker to resume
//!      the prior branch. `allow_dirty` also hardens the workspace-lease
//!      fallback (`crate::coordinator`'s `lease_workspace_with_fallback`):
//!      a failed lease on the preferred workspace fails the dispatch
//!      outright instead of silently landing on a different, clean
//!      workspace, which would strand the uncommitted work.
//!   5. **Escalate** (permanent / unrecognised / retry cap reached):
//!      raise a `WorkAttentionItem` and stop. The orphan-active sweep
//!      excludes work items with an open recovery attention item
//!      (`list_orphan_active_candidates`), so a non-retryable failure is
//!      not blindly re-dispatched.
//!
//! ## Verifying continuation, not just dispatch
//!
//! A nudge or an orphan+respawn is a bare resume: the CLI comes up (or a
//! runtime message is injected) but that alone is not proof the worker
//! is actually doing anything — a resumed session can itself park with
//! no activity (e.g. it hits a permission/notification prompt before
//! ever processing a turn). Left undetected, that reads as `{resumed:
//! 1}` "success" forever, because
//! [`crate::transient_error::extract_worker_error`] only recognises a
//! *trailing API error*; an empty, inert transcript matches neither
//! "errored" nor "recovered," so the old logic just skipped it
//! (`no_error_skipped`) on every later pass too.
//!
//! Every pass, executions with `transient_failure_count > 0` (i.e. this
//! execution is itself the product of an earlier auto-resume) are also
//! checked with [`crate::transient_error::transcript_shows_no_activity`].
//! If the transcript is still empty of any progress or error entry past
//! the grace period, the sweep treats that exactly like a fresh
//! transient error: it re-runs the same nudge → orphan+respawn →
//! escalate decision using the execution's *existing*
//! `transient_failure_count` as `prior_attempts`. This closes two gaps
//! at once: the attempt counter is consumed by a verified-failed
//! recovery instead of being abandoned after one optimistic pass, and a
//! resume that never actually continued eventually escalates for human
//! attention instead of sitting `waiting_for_input` until an unrelated
//! sweep reaps it.
//!
//! ## Freeing the slot means freeing the PANE, not just the bookkeeping
//!
//! Both terminal paths here — orphan+respawn and escalate — hand the
//! worker's slot back to the pool. Doing that by clearing engine-side
//! bookkeeping alone is not enough, and was the direct cause of a
//! mass-wedge: on a 529-Overloaded burst, eleven slots across the
//! interactive and review pools were released engine-side while the app
//! went on hosting a live `claude` pane in every one of them. The pool
//! believed the slots free, each redispatch hit `SpawnWorkerPane` and was
//! rejected `SlotBusy` ("requested slot already hosts a pane (engine/app
//! slot desync, not capacity)"), and the only mechanism that frees such a
//! pane — [`crate::husk_pane_sweep`] — refused to act because eleven
//! confirmed husks in one pass trips its mass-retirement circuit breaker.
//! Nothing broke the loop: the panes stayed, the slots stayed desynced,
//! the resume executions never landed, and the cube workspace leases
//! stayed pinned behind them.
//!
//! So [`release_slot_and_pane`] tears the pane down through the canonical
//! [`crate::app::ServerState::release_worker_pane`] teardown (injected as
//! [`crate::completion::WorkerPaneReleaser`], the same seam the completion
//! path and `bossctl agents stop` use) BEFORE any slot is handed back.
//! Ordering matters in exactly one direction: a slot freed while its pane
//! is still up is an invitation for the next dispatch to be rejected, so
//! the pane always goes first. This also kills the wedged `claude`
//! process, which the bookkeeping-only release never did — leaving the old
//! process alive in the workspace the resume execution is about to
//! re-lease with `allow_dirty`.
//!
//! This does not weaken the husk sweep's breaker in any way; it removes
//! the burst that was feeding it. The breaker remains the backstop for the
//! residual case it was built for — a pane teardown that the engine
//! believes succeeded but that never reached the app.
//!
//! ## No infinite loop
//!
//! The retry cap and exponential backoff live in
//! [`crate::transient_error::RecoveryPolicy`]; after the cap the sweep
//! escalates instead of resuming. The incremented
//! `transient_failure_count` is carried across resume executions, so the
//! cap holds across the whole chain, not per-execution. A nudge does NOT
//! consume a retry slot — the orphan+respawn cap stays at 3 — but each
//! execution only gets one nudge attempt per engine session before the
//! sweep falls back to orphan+respawn.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use boss_protocol::WorkerActivity;

use crate::completion::{PaneReleaseOutcome, WorkerPaneReleaser};
use crate::coordinator::{ExecutionCoordinator, worker_id_for_slot};
use crate::dispatch_events::{DispatchEvent, DispatchEventSink, Outcome, Stage};
use crate::driver::{DriverRegistry, WorkerErrorClass};
use crate::live_worker_state::LiveWorkerStateRegistry;
use crate::transient_error::{
    ErrorClass, EscalateReason, RecoveryDecision, RecoveryPolicy, extract_worker_error, transcript_shows_no_activity,
};
use crate::work::{ATTENTION_KIND_RECOVERY_EXHAUSTED, ATTENTION_KIND_RECOVERY_PERMANENT, WorkDb};

/// How often the sweep runs.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

/// Skip executions whose `started_at` is within this many seconds (or
/// not yet recorded). Guards against acting on a worker that just
/// spawned or just hit a blip claude may still be retrying internally —
/// the transcript only carries the *final* API error after claude gives
/// up, but the grace keeps us from racing a fresh dispatch.
pub const RECOVERY_GRACE_SECS: i64 = 60;

/// Only the tail of the transcript matters (we want the last entry).
/// Reading a bounded suffix keeps the sweep cheap even for multi-MB
/// transcripts. `pub(crate)` so `events_socket`'s hook-ingress publisher
/// can apply the same bound when checking for a trailing API error at
/// `Stop`-hook time (see [`crate::events_socket::publish_hook_derived_events`]).
pub(crate) const TRANSCRIPT_TAIL_MAX_BYTES: u64 = 256 * 1024;

/// Clip error strings to this many bytes before putting them on a
/// dispatch event or attention item.
const ERROR_CLIP_BYTES: usize = 240;

/// Inject text into a live worker's REPL without tearing it down.
/// The recovery sweep uses this to nudge an idle-but-wedged worker
/// before falling back to the heavier orphan+respawn path.
#[async_trait]
pub trait WorkerNudger: Send + Sync {
    async fn nudge_worker(&self, run_id: &str, text: String) -> Result<(), String>;

    /// Push the current `LiveWorkerState` snapshot to UI subscribers
    /// (the macOS app's Agents tab, `bossctl agents list --json` topic
    /// subscribers, …). Called after a direct registry write — e.g.
    /// [`LiveWorkerStateRegistry::set_recovery_status`] — that bypasses
    /// the normal hook-driven `apply_event` + broadcast path, so the
    /// recovery banner shows up promptly instead of waiting for the
    /// next unrelated broadcast. Default no-op keeps
    /// [`NoopWorkerNudger`] and other test doubles trivial.
    async fn broadcast_live_states(&self) {}
}

/// No-op nudger used in tests and contexts without an app session.
/// Always returns `Err`, which causes the sweep to fall through to
/// the orphan+respawn path — preserving pre-nudge test behaviour.
pub struct NoopWorkerNudger;

#[async_trait]
impl WorkerNudger for NoopWorkerNudger {
    async fn nudge_worker(&self, _run_id: &str, _text: String) -> Result<(), String> {
        Err("no nudger configured".into())
    }
}

/// Counts from one pass; logged at `info` when anything happened.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TransientRecoveryOutcome {
    /// Workers sent a runtime nudge (alive-idle path).
    pub nudged: usize,
    /// Workers orphaned and re-queued via the full orphan+respawn path.
    pub resumed: usize,
    pub escalated: usize,
    pub grace_skipped: usize,
    pub no_error_skipped: usize,
}

impl TransientRecoveryOutcome {
    fn has_activity(&self) -> bool {
        self.nudged > 0 || self.resumed > 0 || self.escalated > 0
    }
}

/// Collaborators and policy shared across a transient-recovery sweep,
/// bundled so [`run_one_pass`] stays under clippy's argument-count
/// threshold. The mutable `nudged_executions` set and the injected clock
/// remain explicit parameters of [`run_one_pass`] since they are per-call
/// state rather than shared dependencies.
#[derive(bon::Builder)]
pub struct RecoveryContext<'a> {
    pub work_db: &'a WorkDb,
    pub live_states: &'a LiveWorkerStateRegistry,
    pub coordinator: Arc<ExecutionCoordinator>,
    pub dispatch_events: &'a dyn DispatchEventSink,
    pub policy: &'a RecoveryPolicy,
    pub nudger: &'a dyn WorkerNudger,
    /// Tears down the app-hosted pane behind a slot this sweep is about to
    /// hand back to the pool. Not optional in production: releasing the
    /// slot without it is what wedged eleven slots on a 529 burst (see the
    /// module docs) — the pool frees the slot, the app keeps the pane, and
    /// every redispatch into that slot is rejected `SlotBusy`.
    pub pane_releaser: &'a dyn WorkerPaneReleaser,
}

/// Spawn a tokio task that runs [`run_one_pass`] forever at `interval`,
/// firing immediately on spawn.
pub fn spawn_loop(
    work_db: Arc<WorkDb>,
    live_states: Arc<LiveWorkerStateRegistry>,
    coordinator: Arc<ExecutionCoordinator>,
    dispatch_events: Arc<dyn DispatchEventSink>,
    nudger: Arc<dyn WorkerNudger>,
    pane_releaser: Arc<dyn WorkerPaneReleaser>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    let policy = RecoveryPolicy::default();
    tokio::spawn(async move {
        // Execution IDs that received a runtime nudge this engine session.
        // On the next sweep pass, if the error is still present, we skip
        // the nudge and fall through to orphan+respawn. Keyed by the
        // original execution ID (not the replacement), so stale entries
        // from completed/orphaned executions are harmless.
        let mut nudged_executions: HashSet<String> = HashSet::new();
        loop {
            let now = boss_engine_utils::epoch_time::now_epoch_secs();
            let cx = RecoveryContext {
                work_db: work_db.as_ref(),
                live_states: live_states.as_ref(),
                coordinator: coordinator.clone(),
                dispatch_events: dispatch_events.as_ref(),
                policy: &policy,
                nudger: nudger.as_ref(),
                pane_releaser: pane_releaser.as_ref(),
            };
            let outcome = run_one_pass(&cx, &mut nudged_executions, now).await;
            if outcome.has_activity() {
                tracing::info!(
                    nudged = outcome.nudged,
                    resumed = outcome.resumed,
                    escalated = outcome.escalated,
                    "transient-recovery sweep: pass complete",
                );
            }
            tokio::time::sleep(interval).await;
        }
    })
}

/// Run a single recovery pass. `now_epoch_secs` is injected so tests
/// can pin the clock for the grace guard.
///
/// `nudger` is used to send a runtime message into a live idle worker's
/// REPL instead of tearing it down. `nudged_executions` persists across
/// calls (owned by the spawn loop) so the sweep knows which executions
/// have already been nudged and should proceed to orphan+respawn.
pub async fn run_one_pass(
    cx: &RecoveryContext<'_>,
    nudged_executions: &mut HashSet<String>,
    now_epoch_secs: i64,
) -> TransientRecoveryOutcome {
    let &RecoveryContext {
        work_db,
        live_states,
        dispatch_events,
        policy,
        nudger,
        pane_releaser,
        ..
    } = cx;
    let coordinator = cx.coordinator.clone();

    let mut outcome = TransientRecoveryOutcome::default();
    let grace_cutoff = now_epoch_secs - RECOVERY_GRACE_SECS;

    for state in live_states.snapshot() {
        // Skip slots that are actively progressing or not yet up — only
        // a wedged-idle / errored / terminated slot can be stalled on an
        // API error. (A working slot's last transcript entry is a tool
        // call, never the trailing error, so it would be filtered out
        // anyway; this just avoids the file read.)
        if !should_inspect(state.activity) {
            continue;
        }

        let execution_id = state.run_id.clone();
        let execution = match work_db.get_execution(&execution_id) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(execution_id, ?err, "transient-recovery: execution lookup failed");
                continue;
            }
        };
        // Terminal executions are settled (completion path / dead-PID
        // sweep / a prior recovery pass handled them).
        if execution.status.is_terminal() {
            continue;
        }

        // Grace guard: don't act on a worker that only just started.
        let started_epoch = execution.started_epoch();
        match started_epoch {
            Some(t) if t < grace_cutoff => {}
            _ => {
                outcome.grace_skipped += 1;
                continue;
            }
        }

        // Ground truth: the transcript. No path → no signal → leave it
        // for the other reconcilers.
        let Some(transcript_path) = work_db.latest_transcript_path(&execution_id).ok().flatten() else {
            outcome.no_error_skipped += 1;
            continue;
        };
        let lines = read_transcript_tail(&transcript_path, TRANSCRIPT_TAIL_MAX_BYTES).await;
        let prior_attempts = execution.transient_failure_count.max(0) as u32;

        // Two shapes of "this worker is stalled" are worth acting on:
        //
        //  1. A trailing API-error transcript entry — the classic case
        //     this module was built for.
        //  2. Verification failure: `prior_attempts > 0` means this
        //     execution IS ITSELF the product of an earlier auto-resume
        //     (nudge or orphan+respawn), and its transcript shows no
        //     activity whatsoever. A bare resume is not, by itself,
        //     proof of continuation — the CLI parks at its prompt by
        //     design, so a resumed session that never produced a single
        //     hook-visible event never actually got the continuation
        //     prompt into the model. Gating on `prior_attempts > 0` (and
        //     not just "empty transcript") keeps this from misfiring on
        //     an ordinary fresh execution that simply hasn't started
        //     yet — the grace guard above already covers that case for
        //     the common short delay, but a resume chain deserves this
        //     extra check even past grace because leaving it silently
        //     parked is exactly the bug this closes.
        let (error_text, class, stall_reason) = match extract_worker_error(&lines) {
            Some(text) => {
                // Route through the resolved driver's ControlVerbs
                // classifier — never call a provider-specific helper
                // directly. An unresolvable driver fails closed to
                // Indeterminate (bounded retry, then escalate) rather
                // than inventing Claude's table for every backend.
                let class = classify_error_for_execution(work_db, &execution_id, &text);
                (text, class, "api_error")
            }
            None if prior_attempts > 0 && transcript_shows_no_activity(&lines) => (
                "auto-resume produced no worker activity: no continuation prompt reached the session".to_owned(),
                ErrorClass::Transient,
                "no_activity",
            ),
            None => {
                // No trailing API error, and either this isn't a resume
                // chain or the transcript shows real activity: worker
                // either finished cleanly or recovered on its own. Not
                // ours to touch.
                outcome.no_error_skipped += 1;
                continue;
            }
        };

        let decision = policy.decide(class, prior_attempts);
        let clipped = clip(&error_text, ERROR_CLIP_BYTES);
        let work_item_id = state
            .work_item_id
            .clone()
            .unwrap_or_else(|| execution.work_item_id.clone());

        match decision {
            RecoveryDecision::Resume { attempt, backoff } => {
                // Prefer a cheap runtime nudge when the worker is alive
                // and idle. Only nudge once per execution per engine
                // session: if it didn't clear the error by the next
                // sweep, fall through to orphan+respawn.
                let already_nudged = nudged_executions.remove(&execution_id);
                let try_nudge = !already_nudged && state.activity == WorkerActivity::Idle;

                if try_nudge {
                    let msg = if stall_reason == "no_activity" {
                        "Your previous auto-resume did not reach the model: no continuation \
                         prompt took effect and the session parked with no activity. Please \
                         check for any in-progress work (e.g. `jj diff`) and continue the task \
                         from where it left off.\n"
                            .to_owned()
                    } else {
                        format!(
                            "Your previous turn ended on a transient Claude API error. \
                             Please retry the last step.\n\nError: {clipped}\n"
                        )
                    };
                    match nudger.nudge_worker(&execution_id, msg).await {
                        Ok(()) => {
                            nudged_executions.insert(execution_id.clone());
                            // Visibility (not just the transcript-tail signal):
                            // write a recovery banner directly onto the live
                            // slot so `bossctl agents list` / the Agents-tab
                            // subtitle read "recovering from API error
                            // (attempt N/M)" instead of a bare `idle` that
                            // reads as "worker finished, waiting on nothing in
                            // particular". apply_event clears this the moment
                            // the worker's next hook event proves it resumed.
                            if live_states.set_recovery_status(
                                state.slot_id,
                                Some(format!(
                                    "recovering from API error (attempt {attempt}/{max})",
                                    max = policy.max_attempts(),
                                )),
                            ) {
                                nudger.broadcast_live_states().await;
                            }
                            tracing::info!(
                                execution_id,
                                work_item_id = %work_item_id,
                                error = %clipped,
                                "transient-recovery: nudged live idle worker; will re-check next sweep",
                            );
                            dispatch_events
                                .emit(
                                    DispatchEvent::new(Stage::TransientRecoveryNudge, Outcome::Ok, &execution_id)
                                        .with_work_item(&work_item_id)
                                        .with_details(serde_json::json!({
                                            "error": clipped,
                                            "class": stall_reason,
                                        })),
                                )
                                .await;
                            outcome.nudged += 1;
                            continue; // leave slot and execution intact
                        }
                        Err(nudge_err) => {
                            tracing::info!(
                                execution_id,
                                work_item_id = %work_item_id,
                                nudge_err,
                                "transient-recovery: nudge not available; falling back to orphan+respawn",
                            );
                            // Fall through to the orphan+respawn path below.
                        }
                    }
                }

                // --- Orphan+respawn path ---
                let dispatch_not_before = now_epoch_secs + backoff.as_secs() as i64;
                let reason = format!(
                    "{stall_reason} (auto-resume attempt {attempt}/{max}): {clipped}",
                    max = policy.max_attempts(),
                );
                if let Err(err) =
                    work_db.request_resume_execution(&execution_id, attempt as i64, dispatch_not_before, &reason)
                {
                    tracing::warn!(
                        execution_id,
                        ?err,
                        "transient-recovery: failed to create resume execution; skipping",
                    );
                    continue;
                }
                tracing::info!(
                    execution_id,
                    work_item_id = %work_item_id,
                    attempt,
                    max_attempts = policy.max_attempts(),
                    backoff_secs = backoff.as_secs(),
                    error = %clipped,
                    stall_reason,
                    "transient-recovery: worker stalled; auto-resuming on same workspace",
                );
                release_slot_and_pane(&coordinator, live_states, pane_releaser, &execution_id, state.slot_id).await;
                crate::reconcile_audit::append_reconcile_audit_best_effort(
                    work_db,
                    &work_item_id,
                    now_epoch_secs,
                    &format!(
                        "{stall_reason}; auto-resuming attempt {attempt}/{max} after {secs}s backoff",
                        max = policy.max_attempts(),
                        secs = backoff.as_secs(),
                    ),
                );
                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::TransientRecovery, Outcome::Ok, &execution_id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "attempt": attempt,
                                "max_attempts": policy.max_attempts(),
                                "backoff_secs": backoff.as_secs(),
                                "class": stall_reason,
                                "error": clipped,
                            })),
                    )
                    .await;

                // Defer a kick until the backoff window expires so the
                // resume dispatches promptly, plus an immediate kick so
                // the coordinator notices the freed slot.
                coordinator.kick();
                let coordinator = coordinator.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(backoff).await;
                    coordinator.kick();
                });
                outcome.resumed += 1;
            }
            RecoveryDecision::Escalate { reason } => {
                let kind = match reason {
                    EscalateReason::Permanent => ATTENTION_KIND_RECOVERY_PERMANENT,
                    EscalateReason::RetriesExhausted => ATTENTION_KIND_RECOVERY_EXHAUSTED,
                };
                // The true classified error class, independent of *why* the
                // policy stopped retrying: `RetriesExhausted` covers both a
                // confirmed-transient error that never cleared and an
                // indeterminate one that got the same bounded chances.
                let class_label = class.as_str();
                // Settle the execution so it isn't re-inspected; ignore a
                // race where another reconciler already marked it terminal.
                match work_db.mark_execution_orphaned(
                    &execution_id,
                    &format!("transient-recovery escalation ({}): {clipped}", reason.as_str()),
                ) {
                    Ok(orphaned) => {
                        // Reap termination path: tear down any driver-owned
                        // state outside the workspace.
                        crate::driver_teardown::teardown_driver_workspace(
                            work_db,
                            &execution_id,
                            orphaned.workspace_path.as_deref().map(std::path::Path::new),
                        )
                        .await;
                    }
                    Err(err) => {
                        tracing::debug!(
                            execution_id,
                            ?err,
                            "transient-recovery: execution already terminal at escalation (benign)",
                        );
                    }
                }
                let title = match reason {
                    EscalateReason::RetriesExhausted => "Worker auto-recovery exhausted retries".to_owned(),
                    _ => "Worker hit a non-retryable Claude API error".to_owned(),
                };
                let body = format!(
                    "The engine stopped auto-resuming this work item.\n\n\
                     **Reason:** {reason}\n\n\
                     **Error class:** {class_label}\n\n\
                     **Last worker error:** {clipped}\n\n\
                     **Transient resume attempts already made:** {prior_attempts} / {max}\n\n\
                     Resolve this attention item once the underlying problem is fixed to \
                     allow the work item to be re-dispatched.",
                    reason = reason.as_str(),
                    max = policy.max_attempts(),
                );
                if let Err(err) = work_db.upsert_work_item_attention(&work_item_id, kind, &title, &body) {
                    tracing::warn!(
                        execution_id,
                        work_item_id = %work_item_id,
                        ?err,
                        "transient-recovery: failed to raise attention item",
                    );
                }
                tracing::warn!(
                    execution_id,
                    work_item_id = %work_item_id,
                    reason = reason.as_str(),
                    class = class_label,
                    error = %clipped,
                    "transient-recovery: escalating worker for human attention (not auto-retried)",
                );
                release_slot_and_pane(&coordinator, live_states, pane_releaser, &execution_id, state.slot_id).await;
                dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::TransientRecoveryExhausted, Outcome::Error, &execution_id)
                            .with_work_item(&work_item_id)
                            .with_details(serde_json::json!({
                                "reason": reason.as_str(),
                                "class": class_label,
                                "prior_attempts": prior_attempts,
                                "max_attempts": policy.max_attempts(),
                                "error": clipped,
                            })),
                    )
                    .await;
                coordinator.kick();
                outcome.escalated += 1;
            }
        }
    }

    outcome
}

/// True for slot states a stalled-on-error worker can be in. A
/// `Working`/`Spawning` slot is actively progressing (or not yet up).
/// Resolve the execution's driver and ask it to classify `text`.
///
/// Falls closed to [`ErrorClass::Indeterminate`] when the driver slug is
/// unknown or unregistered — never invents a provider-specific table.
fn classify_error_for_execution(work_db: &WorkDb, execution_id: &str, text: &str) -> ErrorClass {
    let slug = match work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            tracing::debug!(
                execution_id,
                "transient-recovery: no driver recorded for execution; treating error as indeterminate",
            );
            return ErrorClass::Indeterminate;
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                ?err,
                "transient-recovery: driver lookup failed; treating error as indeterminate",
            );
            return ErrorClass::Indeterminate;
        }
    };
    match DriverRegistry::default().get(&slug) {
        Some(driver) => worker_error_class_to_policy(driver.classify_error(text)),
        None => {
            tracing::warn!(
                execution_id,
                driver = %slug,
                "transient-recovery: driver slug not in registry; treating error as indeterminate",
            );
            ErrorClass::Indeterminate
        }
    }
}

fn worker_error_class_to_policy(class: WorkerErrorClass) -> ErrorClass {
    match class {
        WorkerErrorClass::Transient => ErrorClass::Transient,
        WorkerErrorClass::Permanent => ErrorClass::Permanent,
        WorkerErrorClass::Indeterminate => ErrorClass::Indeterminate,
    }
}

fn should_inspect(activity: WorkerActivity) -> bool {
    matches!(
        activity,
        WorkerActivity::Idle | WorkerActivity::WaitingForInput | WorkerActivity::Errored | WorkerActivity::Terminated
    )
}

/// Hand `slot_id` back to the pool — pane first, bookkeeping second.
///
/// The pane teardown is the whole point. `release_pane` routes to
/// [`crate::app::ServerState::release_worker_pane`], which asks the app to
/// destroy the libghostty surface, signals the worker's process group
/// (SIGTERM→SIGKILL) as an engine-side backstop, releases the pool claim,
/// drops the live-state entry, and broadcasts the new snapshot. Doing all of
/// that under one call is what keeps the engine's view of "slot N is free"
/// and the app's view of "slot N hosts nothing" from diverging.
///
/// **Ordering is load-bearing and one-directional: pane down, then slot
/// free.** The inverse — what this path did before, and what a related bug
/// in this subsystem did earlier — publishes a free slot the app will still
/// reject: the scheduler is kicked, claims the slot, calls
/// `SpawnWorkerPane`, and gets `SlotBusy` from an app that is still hosting
/// the old pane. That desync is unobservable from engine state alone (every
/// engine-driven sweep reads bookkeeping the release already cleared), so it
/// persists until the husk sweep retires the pane — and on a mass event the
/// husk sweep's circuit breaker declines to, which is the wedge this closes.
///
/// [`PaneReleaseOutcome::NoLiveWorker`] means the run had no slot mapping,
/// so `release_worker_pane` returned before touching *any* engine state:
/// nothing was torn down and nothing was freed. Only then do we free the
/// slot ourselves, which is exactly what this path did unconditionally
/// before. On `Reaped` a second release would be redundant and racy — the
/// scheduler has already been kicked and may have re-claimed the slot for a
/// fresh execution by the time we got control back.
async fn release_slot_and_pane(
    coordinator: &Arc<ExecutionCoordinator>,
    live_states: &LiveWorkerStateRegistry,
    pane_releaser: &dyn WorkerPaneReleaser,
    execution_id: &str,
    slot_id: u8,
) {
    if pane_releaser.release_pane(execution_id).await == PaneReleaseOutcome::Reaped {
        tracing::info!(
            execution_id,
            slot_id,
            "transient-recovery: tore down the worker's app-hosted pane before freeing its slot",
        );
        return;
    }
    tracing::debug!(
        execution_id,
        slot_id,
        "transient-recovery: no pane was mapped for this run; freeing the slot's engine-side \
         bookkeeping directly",
    );
    // Drop the live-state entry before releasing the pool claim — see the
    // matching comment in `dead_pid_sweep::reap_dead_execution`. Leaving it
    // behind desyncs the pool's "free" bookkeeping from the engine's own
    // live-worker view (masking the slot from `husk_pane_sweep` and
    // reporting a stalled-and-abandoned run as still live), which is
    // exactly the divergence that lets a later dispatch re-claim this slot
    // and get rejected `SlotBusy` by an app that still hosts a pane here.
    live_states.release_slot(slot_id);
    // Use worker_id_for_slot (not WorkerPool::worker_id_for_slot) so
    // automation-pool slots (> MAX_WORKER_POOL_SIZE) produce the
    // "auto-worker-N" prefix and release_worker_and_kick routes to
    // the correct pool via pool_for_worker_id.
    let worker_id = worker_id_for_slot(slot_id);
    coordinator.release_worker_and_kick(&worker_id, None).await;
}

/// Read the last `max_bytes` of a transcript file and parse the
/// complete JSONL lines within. Tolerant: a missing file, an unreadable
/// file, or malformed lines yield an empty/partial vec rather than an
/// error — recovery should never crash on a bad transcript. `pub(crate)`
/// so `events_socket`'s hook-ingress publisher can reuse the same
/// ground-truth read instead of duplicating it (see
/// [`crate::events_socket::publish_hook_derived_events`]).
pub(crate) async fn read_transcript_tail(path: &str, max_bytes: u64) -> Vec<Value> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return Vec::new(),
    };
    let (seek_to, drop_first_partial) = if len > max_bytes {
        (len - max_bytes, true)
    } else {
        (0, false)
    };
    if file.seek(SeekFrom::Start(seek_to)).await.is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).await.is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut iter = text.lines();
    // If we seeked into the middle of the file the first line is likely
    // a partial JSON fragment — drop it.
    if drop_first_partial {
        iter.next();
    }
    iter.filter_map(|line| {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            None
        } else {
            serde_json::from_str::<Value>(trimmed).ok()
        }
    })
    .collect()
}

/// Trim `s`, collapse embedded newlines to spaces, then clip to `max_bytes`
/// on a char boundary. The one-line normalization is specific to error
/// snippets; the byte-bounded clip is shared.
fn clip(s: &str, max_bytes: usize) -> String {
    let one_line = s.trim().replace('\n', " ");
    boss_engine_utils::string_clip::clip_to_bytes(&one_line, max_bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Write;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use boss_protocol::WorkItemBinding;
    use tempfile::TempDir;

    use super::*;
    use crate::dispatch_events::RecordingDispatchEventSink;
    use crate::test_support::*;
    use crate::transient_error::RecoveryPolicy;
    use crate::work::{ExecutionStatus, WorkDb};

    // ─── stubs ────────────────────────────────────────────────────────
    // `NoopCube` / `NoopRunner` come from `crate::test_support::*`.

    /// Records which run_ids were nudged. Used to assert nudge behaviour
    /// without needing a real app session.
    struct RecordingNudger {
        nudged: tokio::sync::Mutex<Vec<String>>,
    }

    impl RecordingNudger {
        fn new() -> Self {
            Self {
                nudged: tokio::sync::Mutex::new(Vec::new()),
            }
        }

        async fn nudged_ids(&self) -> Vec<String> {
            self.nudged.lock().await.clone()
        }
    }

    #[async_trait]
    impl WorkerNudger for RecordingNudger {
        async fn nudge_worker(&self, run_id: &str, _text: String) -> Result<(), String> {
            self.nudged.lock().await.push(run_id.to_owned());
            Ok(())
        }
    }

    /// [`WorkerPaneReleaser`] double that models what production's
    /// `ServerState::release_worker_pane` actually does to engine state, in
    /// the same order: tear the pane down, then release the pool claim, then
    /// drop the live-state entry. `NoopWorkerPaneReleaser` reports `Reaped`
    /// without touching anything, which would let a test "pass" while the
    /// slot stayed claimed forever — the precise failure this module is
    /// being fixed for.
    ///
    /// Records `(run_id, slot_id)` per release, plus whether the pool claim
    /// was still held when the pane came down, so a test can assert the
    /// pane-before-slot ordering rather than just the end state.
    pub(super) struct RecordingPaneReleaser {
        live_states: Arc<LiveWorkerStateRegistry>,
        coordinator: Arc<ExecutionCoordinator>,
        released: tokio::sync::Mutex<Vec<(String, u8)>>,
        claim_held_at_pane_teardown: tokio::sync::Mutex<Vec<bool>>,
    }

    impl RecordingPaneReleaser {
        pub(super) fn new(live_states: Arc<LiveWorkerStateRegistry>, coordinator: Arc<ExecutionCoordinator>) -> Self {
            Self {
                live_states,
                coordinator,
                released: tokio::sync::Mutex::new(Vec::new()),
                claim_held_at_pane_teardown: tokio::sync::Mutex::new(Vec::new()),
            }
        }

        pub(super) async fn released(&self) -> Vec<(String, u8)> {
            self.released.lock().await.clone()
        }

        pub(super) async fn claim_held_at_pane_teardown(&self) -> Vec<bool> {
            self.claim_held_at_pane_teardown.lock().await.clone()
        }
    }

    #[async_trait]
    impl WorkerPaneReleaser for RecordingPaneReleaser {
        async fn release_pane(&self, run_id: &str) -> PaneReleaseOutcome {
            let Some(state) = self
                .live_states
                .snapshot()
                .into_iter()
                .find(|state| state.run_id == run_id)
            else {
                // No slot mapping — production returns before touching any
                // engine state, which is what makes the caller's fallback
                // release necessary.
                return PaneReleaseOutcome::NoLiveWorker;
            };
            // The pane comes down first. Record whether the pool still
            // believed the slot occupied at that moment: if a caller had
            // already freed the claim, the scheduler could have handed this
            // slot to a fresh dispatch that the app would then reject
            // `SlotBusy`.
            let claimed = self
                .coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .iter()
                .any(|id| id == run_id);
            self.claim_held_at_pane_teardown.lock().await.push(claimed);
            self.released.lock().await.push((run_id.to_owned(), state.slot_id));
            let worker_id = worker_id_for_slot(state.slot_id);
            self.coordinator.release_worker_and_kick(&worker_id, None).await;
            self.live_states.release_slot(state.slot_id);
            PaneReleaseOutcome::Reaped
        }
    }

    // ─── helpers ──────────────────────────────────────────────────────

    /// Create a `running` execution with a backdated `started_at` (past
    /// the grace window) and a run whose transcript is `transcript_path`.
    fn create_running_execution(
        db: &WorkDb,
        work_item_id: &str,
        transcript_path: &str,
        prior_transient_failures: i64,
    ) -> String {
        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id)
                    .preferred_workspace_id("mono-agent-007")
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "mono-agent-007",
            "/tmp/mono-agent-007",
        )
        .unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, transcript_path)
            .unwrap();
        if prior_transient_failures > 0 {
            db.force_transient_failure_count_for_test(&execution.id, prior_transient_failures)
                .unwrap();
        }
        let old_started = boss_engine_utils::epoch_time::now_epoch_secs().saturating_sub(600);
        db.force_started_at_for_test(&execution.id, old_started).unwrap();
        execution.id
    }

    fn write_transcript(dir: &TempDir, name: &str, lines: &[&str]) -> String {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        path.to_string_lossy().into_owned()
    }

    fn register_idle_slot(live_states: &LiveWorkerStateRegistry, slot_id: u8, execution_id: &str, work_item_id: &str) {
        live_states.register_spawn(
            slot_id,
            execution_id,
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: work_item_id.to_owned(),
                work_item_name: "test chore".to_owned(),
                execution_id: execution_id.to_owned(),
            }),
        );
        // Drive Spawning → Idle via a Stop event (no pending notification).
        live_states.apply_event(
            slot_id,
            &boss_protocol::WorkerEvent::Stop {
                session_id: "s".to_owned(),
                stop_hook_active: false,
                stop_reason: boss_protocol::StopReason::Completed,
            },
        );
    }

    const SOCKET_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: The socket connection was closed unexpectedly."}]}}"#;
    const AUTH_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: 401 authentication_error: invalid x-api-key"}]}}"#;
    const CONNECTION_REFUSED_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: Unable to connect to API (ConnectionRefused)"}]}}"#;
    const UNRECOGNIZED_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: something we have never seen before"}]}}"#;
    /// Verbatim shape of the error behind the mass-wedge incident this
    /// module's pane teardown was added for — an HTTP 529 Overloaded burst
    /// that stalled eleven workers at once.
    const OVERLOADED_ERROR_LINE: &str = r#"{"type":"assistant","isApiErrorMessage":true,"message":{"role":"assistant","content":[{"type":"text","text":"API Error: 529 Overloaded. This is a server-side issue, usually temporary — try again in a moment."}]}}"#;
    const NORMAL_LINE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on the task"}]}}"#;

    fn now() -> i64 {
        boss_engine_utils::epoch_time::now_epoch_secs()
    }

    /// Build the [`RecoveryContext`] shared by nearly every test, run one
    /// recovery pass, and return the outcome together with the sink so
    /// callers can still assert on `sink.events()`. The `nudger` and the
    /// `nudged` set vary per test; the policy is always
    /// `RecoveryPolicy::default()` and the clock is `now()`.
    ///
    /// The pane releaser is a [`RecordingPaneReleaser`] over the test's own
    /// live-state registry and coordinator, so slot teardown behaves as it
    /// does in production. Tests that need to inspect what it did call
    /// [`run_pass_with_releaser`] instead.
    async fn run_pass(
        db: &WorkDb,
        live: &Arc<LiveWorkerStateRegistry>,
        coordinator: &Arc<ExecutionCoordinator>,
        nudger: &dyn WorkerNudger,
        nudged: &mut HashSet<String>,
    ) -> (TransientRecoveryOutcome, Arc<RecordingDispatchEventSink>) {
        let releaser = RecordingPaneReleaser::new(live.clone(), coordinator.clone());
        run_pass_with_releaser(db, live, coordinator, nudger, &releaser, nudged).await
    }

    /// [`run_pass`] with the pane releaser supplied by the caller.
    async fn run_pass_with_releaser(
        db: &WorkDb,
        live: &Arc<LiveWorkerStateRegistry>,
        coordinator: &Arc<ExecutionCoordinator>,
        nudger: &dyn WorkerNudger,
        pane_releaser: &dyn WorkerPaneReleaser,
        nudged: &mut HashSet<String>,
    ) -> (TransientRecoveryOutcome, Arc<RecordingDispatchEventSink>) {
        let sink = Arc::new(RecordingDispatchEventSink::new());
        let cx = RecoveryContext {
            work_db: db,
            live_states: live.as_ref(),
            coordinator: coordinator.clone(),
            dispatch_events: sink.as_ref(),
            policy: &RecoveryPolicy::default(),
            nudger,
            pane_releaser,
        };
        let outcome = run_one_pass(&cx, nudged, now()).await;
        (outcome, sink)
    }

    // ─── tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn transient_error_nudges_live_idle_worker() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let nudger = RecordingNudger::new();
        let mut nudged = HashSet::new();
        let (outcome, sink) = run_pass(&db, &live, &coordinator, &nudger, &mut nudged).await;

        // First pass: should nudge, not orphan+respawn.
        assert_eq!(outcome.nudged, 1, "alive idle worker should be nudged first");
        assert_eq!(outcome.resumed, 0, "should not orphan+respawn on first nudge");
        assert_eq!(outcome.escalated, 0);
        assert!(nudged.contains(&exec_id), "execution should be in nudged set");
        assert_eq!(nudger.nudged_ids().await, vec![exec_id.clone()]);

        // Execution is still running (not orphaned).
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);

        // Visibility: the live slot carries a recovery banner naming the
        // attempt so `bossctl agents list` / the Agents-tab subtitle don't
        // read as a plain, unremarkable "idle".
        assert_eq!(
            live.get(slot_id).unwrap().recovery_status.as_deref(),
            Some("recovering from API error (attempt 1/3)"),
        );

        // One transient_recovery_nudge dispatch event.
        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery_nudge");
        assert_eq!(events[0].outcome, "ok");
    }

    #[tokio::test]
    async fn nudged_worker_still_stalled_falls_back_to_orphan_respawn() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        // Pre-populate nudged set to simulate a prior-pass nudge.
        let mut nudged = HashSet::new();
        nudged.insert(exec_id.clone());

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut nudged).await;

        // Second pass: nudge already tried, error still present → orphan+respawn.
        assert_eq!(outcome.resumed, 1, "second pass should orphan+respawn");
        assert_eq!(outcome.nudged, 0);
        assert!(
            !nudged.contains(&exec_id),
            "id removed from nudged set on orphan+respawn"
        );

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        let dead = execs.iter().find(|e| e.id == exec_id).unwrap();
        assert_eq!(dead.status, ExecutionStatus::Orphaned);
        let fresh = execs
            .iter()
            .find(|e| e.id != exec_id && e.status == ExecutionStatus::Ready)
            .expect("expected a fresh ready execution");
        assert_eq!(fresh.preferred_workspace_id.as_deref(), Some("mono-agent-007"));
        assert_eq!(fresh.transient_failure_count, 1);
        assert!(
            fresh.allow_dirty,
            "resume execution must force allow_dirty so cube doesn't wipe the recovered workspace"
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery");
    }

    #[tokio::test]
    async fn transient_error_resumes_on_same_workspace() {
        // NoopWorkerNudger always fails → falls through to orphan+respawn.
        // Exercises the pre-nudge behaviour for contexts where nudge is unavailable.
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.resumed, 1, "noop nudger falls through to orphan+respawn");
        assert_eq!(outcome.escalated, 0);

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        let dead = execs.iter().find(|e| e.id == dead_id).unwrap();
        assert_eq!(dead.status, ExecutionStatus::Orphaned);
        let fresh = execs
            .iter()
            .find(|e| e.id != dead_id && e.status == ExecutionStatus::Ready)
            .expect("expected a fresh ready execution");
        assert_eq!(fresh.preferred_workspace_id.as_deref(), Some("mono-agent-007"));
        assert_eq!(fresh.transient_failure_count, 1);
        assert!(
            fresh.dispatch_not_before.is_some(),
            "resume must be deferred by a backoff window",
        );

        let claimed = coordinator.worker_pool().claimed_execution_ids().await;
        assert!(!claimed.contains(&dead_id));

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery");
        assert_eq!(events[0].outcome, "ok");
    }

    /// The orphan+respawn path must tear the worker's app-hosted pane down,
    /// not merely drop the engine's bookkeeping for its slot. Releasing the
    /// slot alone leaves the app hosting a live pane there, so the resume
    /// execution's `SpawnWorkerPane` is rejected `SlotBusy` and the slot is
    /// wedged until something retires the pane by hand.
    #[tokio::test]
    async fn orphan_respawn_tears_down_the_pane_before_freeing_the_slot() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, OVERLOADED_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let releaser = RecordingPaneReleaser::new(live.clone(), coordinator.clone());
        let (outcome, _sink) = run_pass_with_releaser(
            &db,
            &live,
            &coordinator,
            &NoopWorkerNudger,
            &releaser,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(outcome.resumed, 1);
        assert_eq!(
            releaser.released().await,
            vec![(dead_id.clone(), slot_id)],
            "the stalled worker's pane must be torn down for its slot to be genuinely free",
        );
        assert_eq!(
            releaser.claim_held_at_pane_teardown().await,
            vec![true],
            "the pool claim must still be held when the pane comes down — freeing the slot first \
             publishes a slot the app will reject SlotBusy",
        );
        assert!(
            live.get(slot_id).is_none(),
            "live-state entry must be gone once the pane is down"
        );
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&dead_id),
            "pool claim must be released so the resume execution can be dispatched",
        );
    }

    /// The escalate path frees the slot too, so it has the same obligation:
    /// a worker parked behind an unrecoverable error must not keep a pane
    /// alive on a slot the pool has already advertised as free.
    #[tokio::test]
    async fn escalation_tears_down_the_pane_too() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, AUTH_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let releaser = RecordingPaneReleaser::new(live.clone(), coordinator.clone());
        let (outcome, _sink) = run_pass_with_releaser(
            &db,
            &live,
            &coordinator,
            &NoopWorkerNudger,
            &releaser,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(outcome.escalated, 1);
        assert_eq!(releaser.released().await, vec![(dead_id, slot_id)]);
        assert!(live.get(slot_id).is_none());
    }

    /// A nudge leaves the worker running, so it must NOT touch the pane —
    /// the whole point of the nudge-first path is recovering in place.
    #[tokio::test]
    async fn nudge_does_not_touch_the_pane() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, OVERLOADED_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let releaser = RecordingPaneReleaser::new(live.clone(), coordinator.clone());
        let nudger = RecordingNudger::new();
        let (outcome, _sink) =
            run_pass_with_releaser(&db, &live, &coordinator, &nudger, &releaser, &mut HashSet::new()).await;

        assert_eq!(outcome.nudged, 1);
        assert!(
            releaser.released().await.is_empty(),
            "a nudged worker keeps its pane — recovery happens in place",
        );
        assert!(live.get(slot_id).is_some(), "the slot stays live through a nudge");
    }

    /// When no pane was ever mapped for the run, `release_pane` reports
    /// `NoLiveWorker` having touched nothing — so this path must still free
    /// the slot itself, exactly as it did before pane teardown was added.
    #[tokio::test]
    async fn slot_is_still_freed_when_no_pane_was_mapped() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, OVERLOADED_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        /// Stands in for a run the app has no slot mapping for.
        struct UnmappedPaneReleaser;
        #[async_trait]
        impl WorkerPaneReleaser for UnmappedPaneReleaser {
            async fn release_pane(&self, _run_id: &str) -> PaneReleaseOutcome {
                PaneReleaseOutcome::NoLiveWorker
            }
        }

        let (outcome, _sink) = run_pass_with_releaser(
            &db,
            &live,
            &coordinator,
            &NoopWorkerNudger,
            &UnmappedPaneReleaser,
            &mut HashSet::new(),
        )
        .await;

        assert_eq!(outcome.resumed, 1);
        assert!(live.get(slot_id).is_none(), "live-state entry must still be dropped");
        assert!(
            !coordinator
                .worker_pool()
                .claimed_execution_ids()
                .await
                .contains(&dead_id),
            "pool claim must still be released on the no-pane fallback",
        );
    }

    #[tokio::test]
    async fn permanent_error_escalates_and_does_not_resume() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, AUTH_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.escalated, 1, "permanent error should escalate");
        assert_eq!(outcome.resumed, 0, "permanent error must NOT resume");

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        assert!(
            !execs
                .iter()
                .any(|e| e.id != dead_id && e.status == ExecutionStatus::Ready),
            "permanent error must not create a resume execution",
        );
        let attn = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attn.len(), 1);
        assert_eq!(attn[0].kind, ATTENTION_KIND_RECOVERY_PERMANENT);
        assert_eq!(attn[0].status, "open");

        let candidates = db.list_orphan_active_candidates(0).unwrap();
        assert!(
            !candidates.contains(&work_item_id),
            "escalated item must be excluded from orphan-active redispatch",
        );

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery_exhausted");
    }

    #[tokio::test]
    async fn transient_error_at_cap_escalates_exhausted() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        // Already at the cap (3 prior transient resumes).
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 3);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let (outcome, _sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.escalated, 1, "at cap, must escalate not resume");
        assert_eq!(outcome.resumed, 0);
        let attn = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attn[0].kind, ATTENTION_KIND_RECOVERY_EXHAUSTED);
    }

    /// Regression for the 2026-07-08 sleep/wake incident: a
    /// `ConnectionRefused` error at `prior_attempts == 0` must auto-resume
    /// (first via the cheap nudge path since the worker is idle), not
    /// escalate immediately with zero retries attempted.
    #[tokio::test]
    async fn connection_refused_resumes_instead_of_escalating_at_zero_attempts() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, CONNECTION_REFUSED_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let nudger = RecordingNudger::new();
        let (outcome, _sink) = run_pass(&db, &live, &coordinator, &nudger, &mut HashSet::new()).await;

        assert_eq!(
            outcome.nudged, 1,
            "ConnectionRefused must classify as transient and take the resume path, not escalate"
        );
        assert_eq!(outcome.escalated, 0);
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
        assert!(
            db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty(),
            "no attention item must be raised for a first-sighting connection-refused error"
        );
    }

    /// An error the classifier has never seen before (`Indeterminate`) must
    /// get the same bounded retry budget as a confirmed-transient error —
    /// it must NOT escalate at `prior_attempts == 0` the way a confirmed
    /// `Permanent` error does.
    #[tokio::test]
    async fn unrecognized_error_resumes_before_escalating() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[NORMAL_LINE, UNRECOGNIZED_ERROR_LINE]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let (outcome, _sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(
            outcome.resumed, 1,
            "an unrecognized error must resume (orphan+respawn, nudger unavailable), not escalate at zero attempts"
        );
        assert_eq!(outcome.escalated, 0);
        assert!(db.list_attention_items_for_work_item(&work_item_id).unwrap().is_empty());
    }

    /// Once an unrecognized error's retry budget is genuinely exhausted, it
    /// must still escalate (no infinite loop) — but reported with its true
    /// class (`indeterminate`), distinguishable from a confirmed-transient
    /// exhaustion in the dispatch event / attention item.
    #[tokio::test]
    async fn unrecognized_error_at_cap_escalates_with_indeterminate_class() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[UNRECOGNIZED_ERROR_LINE]);
        let db = Arc::new(db);
        // Already at the cap (3 prior transient resumes).
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 3);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.escalated, 1, "at cap, must escalate not resume");
        assert_eq!(outcome.resumed, 0);
        // Same attention kind as a confirmed-transient exhaustion (both are
        // "we tried, it kept failing") — orphan_sweep already excludes this
        // kind pending human resolution, same as before.
        let attn = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attn[0].kind, ATTENTION_KIND_RECOVERY_EXHAUSTED);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery_exhausted");
        assert_eq!(events[0].details["reason"], "retries_exhausted");
        assert_eq!(
            events[0].details["class"], "indeterminate",
            "the true classified error class must be reported, not conflated with the escalate reason"
        );
    }

    #[tokio::test]
    async fn worker_that_recovered_on_its_own_is_left_alone() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        // Error, then more work → recovered. extract_worker_error → None.
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE, NORMAL_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&dead_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &dead_id, &work_item_id);

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.resumed, 0);
        assert_eq!(outcome.escalated, 0);
        assert_eq!(outcome.no_error_skipped, 1);
        assert_eq!(db.get_execution(&dead_id).unwrap().status, ExecutionStatus::Running);
        assert!(sink.events().await.is_empty());
    }

    // ─── verification: resumed-but-stalled-with-no-activity ───────────

    /// The regression this module now closes: a resumed session (this
    /// execution's `transient_failure_count > 0`, i.e. it is itself the
    /// product of an earlier auto-resume) whose transcript shows no
    /// activity at all must NOT be silently left alone — the sweep must
    /// treat it exactly like a fresh transient error and retry it,
    /// consuming the existing attempt count.
    #[tokio::test]
    async fn resumed_execution_with_no_activity_is_retried() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        // Empty transcript: the resumed session never produced a single
        // hook-visible event.
        let transcript = write_transcript(&dir, "t.jsonl", &[]);
        let db = Arc::new(db);
        // transient_failure_count = 1: this execution IS a resume.
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 1);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(
            outcome.resumed, 1,
            "a resumed session with zero activity must be retried, not silently ignored"
        );
        assert_eq!(outcome.no_error_skipped, 0);
        assert_eq!(outcome.escalated, 0);

        let execs = db.list_executions(Some(&work_item_id)).unwrap();
        let dead = execs.iter().find(|e| e.id == exec_id).unwrap();
        assert_eq!(dead.status, ExecutionStatus::Orphaned);
        let fresh = execs
            .iter()
            .find(|e| e.id != exec_id && e.status == ExecutionStatus::Ready)
            .expect("expected a fresh ready execution");
        assert_eq!(
            fresh.transient_failure_count, 2,
            "the attempt counter must be consumed (1 prior + this verified-failed retry)"
        );
        assert!(fresh.allow_dirty);

        let events = sink.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "transient_recovery");
        assert_eq!(events[0].details["class"], "no_activity");
    }

    /// A brand-new execution (`transient_failure_count == 0`, i.e. never
    /// auto-resumed) with an empty transcript must NOT be mistaken for a
    /// stalled resume — it may simply not have started yet. Only the
    /// resume-chain signal (`transient_failure_count > 0`) triggers the
    /// no-activity check; guards against false positives on ordinary
    /// slow-starting workers.
    #[tokio::test]
    async fn fresh_execution_with_no_activity_is_left_alone() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[]);
        let db = Arc::new(db);
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let (outcome, sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.resumed, 0);
        assert_eq!(outcome.escalated, 0);
        assert_eq!(outcome.no_error_skipped, 1);
        assert_eq!(db.get_execution(&exec_id).unwrap().status, ExecutionStatus::Running);
        assert!(sink.events().await.is_empty());
    }

    /// A resumed session that keeps producing zero activity, pass after
    /// pass, must eventually escalate once the retry cap is hit — the
    /// same "no infinite loop" guarantee the transcript-error path
    /// already has.
    #[tokio::test]
    async fn resumed_execution_with_no_activity_at_cap_escalates() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[]);
        let db = Arc::new(db);
        // Already at the cap (3 prior transient resumes).
        let exec_id = create_running_execution(&db, &work_item_id, &transcript, 3);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        let coordinator = make_coordinator(db.clone(), 2);
        let worker_id = coordinator.worker_pool().claim_worker(&exec_id, None).await.unwrap();
        let slot_id = crate::coordinator::slot_id_from_worker_id(&worker_id).unwrap();
        register_idle_slot(&live, slot_id, &exec_id, &work_item_id);

        let (outcome, _sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(
            outcome.escalated, 1,
            "at cap, a no-activity stall must escalate not resume"
        );
        assert_eq!(outcome.resumed, 0);
        let attn = db.list_attention_items_for_work_item(&work_item_id).unwrap();
        assert_eq!(attn[0].kind, ATTENTION_KIND_RECOVERY_EXHAUSTED);
    }

    #[tokio::test]
    async fn fresh_execution_within_grace_is_skipped() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE]);
        let db = Arc::new(db);

        use boss_protocol::RequestExecutionInput;
        let execution = db
            .request_execution(
                RequestExecutionInput::builder()
                    .work_item_id(work_item_id.clone())
                    .preferred_workspace_id("mono-agent-007")
                    .build(),
            )
            .unwrap();
        db.start_execution_run(
            &execution.id,
            "worker-1",
            "repo-1",
            "lease-1",
            "mono-agent-007",
            "/tmp/mono-agent-007",
        )
        .unwrap();
        db.set_run_transcript_path_if_unset(&execution.id, &transcript).unwrap();
        // started_at = NOW (within grace).
        db.force_started_at_for_test(&execution.id, now()).unwrap();

        let live = Arc::new(LiveWorkerStateRegistry::new());
        register_idle_slot(&live, 1, &execution.id, &work_item_id);
        let coordinator = make_coordinator(db.clone(), 2);

        let (outcome, _sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome.resumed, 0);
        assert_eq!(outcome.escalated, 0);
        assert_eq!(outcome.grace_skipped, 1);
    }

    #[tokio::test]
    async fn actively_working_slot_is_not_inspected() {
        let (dir, db) = open_db();
        let product_id = create_product(&db);
        let work_item_id = create_active_chore(&db, &product_id, "test chore");
        let transcript = write_transcript(&dir, "t.jsonl", &[SOCKET_ERROR_LINE]);
        let db = Arc::new(db);
        let dead_id = create_running_execution(&db, &work_item_id, &transcript, 0);

        let live = Arc::new(LiveWorkerStateRegistry::new());
        live.register_spawn(
            1,
            &dead_id,
            "claude-opus-4-7",
            12345,
            Some(WorkItemBinding {
                work_item_id: work_item_id.clone(),
                work_item_name: "c".to_owned(),
                execution_id: dead_id.clone(),
            }),
        );
        // Drive to Working via PreToolUse — must be skipped.
        live.apply_event(
            1,
            &boss_protocol::WorkerEvent::PreToolUse {
                session_id: "s".to_owned(),
                tool_name: "Bash".to_owned(),
                tool_input: serde_json::json!({}),
            },
        );
        let coordinator = make_coordinator(db.clone(), 2);

        let (outcome, _sink) = run_pass(&db, &live, &coordinator, &NoopWorkerNudger, &mut HashSet::new()).await;

        assert_eq!(outcome, TransientRecoveryOutcome::default());
    }

    #[tokio::test]
    async fn read_transcript_tail_handles_missing_file() {
        let lines = read_transcript_tail("/nonexistent/transcript.jsonl", 1024).await;
        assert!(lines.is_empty());
    }

    #[tokio::test]
    async fn read_transcript_tail_bounds_and_drops_partial_first_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // A long padding line, then a couple of valid JSON lines.
        writeln!(f, "{}", "x".repeat(5000)).unwrap();
        writeln!(f, r#"{{"i":1}}"#).unwrap();
        writeln!(f, r#"{{"i":2}}"#).unwrap();
        drop(f);
        // max_bytes smaller than the file → seek into the padding line,
        // which gets dropped as a partial; the two JSON lines survive.
        let lines = read_transcript_tail(&path.to_string_lossy(), 64).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["i"], 1);
        assert_eq!(lines[1]["i"], 2);
    }

    #[tokio::test]
    async fn read_transcript_tail_parses_all_lines_when_under_max() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("small.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"i":1}}"#).unwrap();
        writeln!(f).unwrap(); // blank line — must be skipped, not parsed
        writeln!(f, r#"{{"i":2}}"#).unwrap();
        writeln!(f, "   ").unwrap(); // whitespace-only line — also skipped
        writeln!(f, r#"{{"i":3}}"#).unwrap();
        drop(f);
        // File is far under max_bytes → no seek, no dropped first line; every
        // complete JSON line is parsed and blank lines are skipped.
        let lines = read_transcript_tail(&path.to_string_lossy(), 1 << 20).await;
        assert_eq!(lines.len(), 3, "blank/whitespace lines must be skipped");
        assert_eq!(lines[0]["i"], 1);
        assert_eq!(lines[1]["i"], 2);
        assert_eq!(lines[2]["i"], 3);
    }

    #[tokio::test]
    async fn read_transcript_tail_skips_malformed_lines() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mixed.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, r#"{{"i":1}}"#).unwrap();
        writeln!(f, "this is not json").unwrap();
        writeln!(f, r#"{{"i":2"#).unwrap(); // truncated/invalid JSON (no closing brace)
        writeln!(f, r#"{{"i":3}}"#).unwrap();
        drop(f);
        // Non-JSON and malformed lines are dropped rather than erroring; the
        // two well-formed lines survive in order.
        let lines = read_transcript_tail(&path.to_string_lossy(), 1 << 20).await;
        assert_eq!(lines.len(), 2, "malformed lines must be skipped, not fatal");
        assert_eq!(lines[0]["i"], 1);
        assert_eq!(lines[1]["i"], 3);
    }

    #[test]
    fn clip_trims_and_collapses_newlines() {
        // Leading/trailing whitespace is trimmed and embedded newlines
        // become single spaces.
        assert_eq!(clip("  hello  ", 100), "hello");
        assert_eq!(clip("line1\nline2\nline3", 100), "line1 line2 line3");
        // Trim happens before newline replacement, so surrounding blank
        // lines are removed entirely.
        assert_eq!(clip("\n  middle  \n", 100), "middle");
    }

    #[test]
    fn clip_returns_short_string_unchanged() {
        // At/under max_bytes → returned as-is, no ellipsis.
        let s = "short error";
        let out = clip(s, s.len());
        assert_eq!(out, s);
        assert!(!out.contains('…'), "no ellipsis when within budget");

        let out = clip("tiny", 100);
        assert_eq!(out, "tiny");
        assert!(!out.contains('…'));
    }

    #[test]
    fn clip_truncates_overlong_ascii_with_ellipsis() {
        let s = "x".repeat(1000);
        let out = clip(&s, 10);
        assert!(out.ends_with('…'), "over-length output must end with ellipsis");
        // The retained prefix (everything before the ellipsis) stays within
        // the byte budget.
        let prefix = out.strip_suffix('…').unwrap();
        assert!(prefix.len() <= 10, "prefix {} bytes must be <= max_bytes", prefix.len());
        assert_eq!(prefix, "x".repeat(10));
    }

    #[test]
    fn clip_truncation_respects_utf8_char_boundary() {
        // '世' is 3 bytes; 10 of them = 30 bytes. max_bytes = 8 lands inside
        // the third character — clip must walk back to a char boundary (6)
        // and must NOT panic.
        let s = "世".repeat(10);
        let out = clip(&s, 8);
        assert!(out.ends_with('…'));
        let prefix = out.strip_suffix('…').unwrap();
        assert_eq!(prefix, "世世", "must walk back to the char boundary at 6");
        assert!(prefix.len() <= 8);

        // 'é' is 2 bytes; an odd max_bytes lands mid-codepoint and must also
        // walk back without panicking.
        let s = "é".repeat(10);
        let out = clip(&s, 5);
        assert!(out.ends_with('…'));
        let prefix = out.strip_suffix('…').unwrap();
        assert_eq!(prefix, "éé", "must walk back to the char boundary at 4");
        assert!(prefix.len() <= 5);
    }

    #[test]
    fn should_inspect_covers_every_worker_activity_variant() {
        // Exhaustive over WorkerActivity so adding a variant forces a
        // deliberate decision here rather than silently defaulting.
        for activity in [
            WorkerActivity::Spawning,
            WorkerActivity::Working,
            WorkerActivity::WaitingForInput,
            WorkerActivity::Idle,
            WorkerActivity::Errored,
            WorkerActivity::Terminated,
        ] {
            let expected = match activity {
                WorkerActivity::Idle
                | WorkerActivity::WaitingForInput
                | WorkerActivity::Errored
                | WorkerActivity::Terminated => true,
                WorkerActivity::Spawning | WorkerActivity::Working => false,
            };
            assert_eq!(
                should_inspect(activity),
                expected,
                "unexpected should_inspect verdict for {activity:?}",
            );
        }
    }
}
