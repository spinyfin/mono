//! The pause-only forced-dispatch override: the read-only admission
//! evaluator (`EvaluateDispatchAdmission`) and the mutating orchestration
//! both `bossctl work start --force` and a confirmed macOS drag-to-Doing
//! drive through `RequestExecutionInput::bypass_dispatch_pause`. See
//! `docs/designs/operator-forced-dispatch-while-dispatch-is-paused.md`.
//!
//! [`ExecutionCoordinator::evaluate_dispatch_admission`] is the ONE
//! reason-producing function: both the read-only preview RPC and
//! [`ExecutionCoordinator::dispatch_with_pause_bypass`]'s mutating refusal
//! path call it, so a confirmation the app renders can never promise
//! something the mutating request doesn't also honor.

use std::collections::HashSet;

use anyhow::bail;
use boss_protocol::{
    ADMISSION_BLOCKER_AUTOSTART_DISABLED, ADMISSION_BLOCKER_CHURN_GUARD_PARKED, ADMISSION_BLOCKER_INELIGIBLE_STATUS,
    ADMISSION_BLOCKER_INTERACTIVE_CONCURRENCY_CAP, ADMISSION_BLOCKER_UNMET_DEPENDENCY, DispatchAdmission,
    DispatchAdmissionBlocker, DispatchAdmissionEntryPoint, DispatchPauseSnapshot, RequestExecutionInput,
};

use super::*;
use crate::work::CancelExecutionOpts;

/// The two blocker codes that a plain (non-forced) `RequestExecution` has
/// always bypassed regardless of `force` — see
/// `request_execution_in_tx_with_live_check`'s unconditional
/// `resolve_attention_kind_in_tx` (churn-guard) and
/// `task_accepts_execution`'s doc comment (autostart, "explicit
/// RequestExecution still creates a ready execution"). The design's
/// non-goals are explicit that a forced explicit start retains those same
/// pre-existing behaviors rather than the new force bit newly authorizing
/// them, so [`DispatchAdmission::blockers`] reports both for transparency
/// (the confirmation UI names every constraint force does not touch) but
/// neither ever causes [`ExecutionCoordinator::dispatch_with_pause_bypass`]
/// to refuse a request that would otherwise succeed.
const INFORMATIONAL_ONLY_BLOCKER_CODES: &[&str] = &[
    ADMISSION_BLOCKER_CHURN_GUARD_PARKED,
    ADMISSION_BLOCKER_AUTOSTART_DISABLED,
];

fn entry_point_label(entry_point: DispatchAdmissionEntryPoint) -> &'static str {
    match entry_point {
        DispatchAdmissionEntryPoint::Cli => "cli",
        DispatchAdmissionEntryPoint::AppDrag => "app_drag",
    }
}

fn describe_blockers(blockers: &[&DispatchAdmissionBlocker]) -> String {
    blockers
        .iter()
        .map(|b| b.message.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

/// Given an already-computed [`DispatchAdmission`], decide whether a
/// pause-only override should proceed — and if not, why. Shared by the
/// synchronous pre-check `handle_move_work_item_on_board` runs before ever
/// touching the board (so a refused drag never flips status in the first
/// place — the "bounce the card back" behavior) and by
/// [`ExecutionCoordinator::dispatch_with_pause_bypass`]'s own authoritative
/// re-check, so the two can never drift on what counts as a refusal — the
/// design's "one reason-producing function" constraint applied one level
/// down from [`ExecutionCoordinator::evaluate_dispatch_admission`] itself.
///
/// Returns `Ok(true)` when the pause is active, overridable, and — if
/// `observed_pause_since_epoch_s` was supplied — still the same generation
/// the caller observed: go ahead and attempt the bypass. `Ok(false)` when
/// no pause is active at all (nothing to override — proceed as an
/// ordinary start, exactly as the design's "pause was lifted" case
/// requires). `Err(reason)` when something refuses outright: a hard
/// blocker, a non-overridable (breaker-origin) pause, or a stale
/// confirmation.
pub fn pause_bypass_decision(
    admission: &DispatchAdmission,
    observed_pause_since_epoch_s: Option<u64>,
) -> std::result::Result<bool, String> {
    let hard_blockers: Vec<&DispatchAdmissionBlocker> = admission
        .blockers
        .iter()
        .filter(|b| !INFORMATIONAL_ONLY_BLOCKER_CODES.contains(&b.code.as_str()))
        .collect();
    if !hard_blockers.is_empty() {
        return Err(format!(
            "blocked even with force: {}",
            describe_blockers(&hard_blockers)
        ));
    }
    if !admission.pause.active {
        return Ok(false);
    }
    if !admission.pause.overridable {
        return Err(format!(
            "dispatch is paused by {} ({}) — this pause is not overridable",
            admission.pause.origin.as_deref().unwrap_or("unknown"),
            admission.pause.reason.as_deref().unwrap_or("no reason recorded"),
        ));
    }
    if let Some(observed) = observed_pause_since_epoch_s
        && admission.pause.paused_since_epoch_s != Some(observed)
    {
        return Err(format!(
            "the dispatch pause changed since it was last observed (now: {}) — re-check and confirm again",
            admission.pause.reason.as_deref().unwrap_or("no reason recorded"),
        ));
    }
    Ok(true)
}

impl ExecutionCoordinator {
    /// Read-only: would `work_item_id` dispatch right now, and if not,
    /// why not? Computes the current dispatch-pause snapshot plus every
    /// blocker in [`INFORMATIONAL_ONLY_BLOCKER_CODES`] and the genuinely
    /// enforced ones (ineligible status, unmet dependencies, the
    /// interactive concurrency cap). No DB writes, no dispatch events —
    /// this never mutates anything, unlike the request path it mirrors.
    pub async fn evaluate_dispatch_admission(&self, work_item_id: &str) -> Result<DispatchAdmission> {
        let facts = self.work_db.dispatch_admission_facts(work_item_id)?;
        let mut blockers = Vec::new();
        if let Some(reason) = &facts.ineligible_reason {
            blockers.push(DispatchAdmissionBlocker {
                code: ADMISSION_BLOCKER_INELIGIBLE_STATUS.to_string(),
                message: reason.clone(),
            });
        }
        if !facts.unmet_dependencies.is_empty() {
            blockers.push(DispatchAdmissionBlocker {
                code: ADMISSION_BLOCKER_UNMET_DEPENDENCY.to_string(),
                message: format!(
                    "blocked by an unmet dependency — gated by [{}]; remove the edge or wait for the prereq to complete",
                    facts.unmet_dependencies.join(", ")
                ),
            });
        }
        if facts.churn_guard_parked {
            blockers.push(DispatchAdmissionBlocker {
                code: ADMISSION_BLOCKER_CHURN_GUARD_PARKED.to_string(),
                message: "churn-guard parked (an explicit start always clears this, force or not)".to_string(),
            });
        }
        if facts.autostart_disabled {
            blockers.push(DispatchAdmissionBlocker {
                code: ADMISSION_BLOCKER_AUTOSTART_DISABLED.to_string(),
                message: "autostart is disabled (an explicit start always bypasses this, force or not)".to_string(),
            });
        }
        // The interactive concurrency cap only governs main-pool work, and
        // only means anything once the row is otherwise eligible.
        if facts.ineligible_reason.is_none() && facts.targets_main_pool {
            let live = self.worker_pool.busy_count().await;
            let cap = self.max_concurrent_interactive_workers();
            if live >= cap {
                blockers.push(DispatchAdmissionBlocker {
                    code: ADMISSION_BLOCKER_INTERACTIVE_CONCURRENCY_CAP.to_string(),
                    message: format!("interactive concurrency cap reached ({live}/{cap} workers live)"),
                });
            }
        }
        let mut pause = self.dispatch_pause_snapshot();
        if facts.exempt_from_operator_pause && pause.overridable {
            // `drain_ready_queue` only holds `paused && !is_review` — an
            // operator-originated pause exempts review-pool executions
            // entirely (see `pool_for_execution` / the pause gate in
            // `coordinator/scheduler.rs`), so this row would dispatch
            // right now regardless of the pause. Report no pause in
            // effect for it at all, per the design doc's "Pools" section:
            // force is normally meaningless for reviews and the
            // evaluation reports that no pause override is needed —
            // rather than a bypassable pause that would let
            // `pause_bypass_decision` record a bogus override.
            pause = DispatchPauseSnapshot::default();
        }
        let would_dispatch = !Self::has_hard_blocker(&blockers) && !pause.active;
        Ok(DispatchAdmission::builder()
            .work_item_id(facts.resolved_work_item_id)
            .would_dispatch(would_dispatch)
            .pause(pause)
            .blockers(blockers)
            .build())
    }

    fn has_hard_blocker(blockers: &[DispatchAdmissionBlocker]) -> bool {
        blockers
            .iter()
            .any(|b| !INFORMATIONAL_ONLY_BLOCKER_CODES.contains(&b.code.as_str()))
    }

    /// The current global dispatch pause, as [`DispatchAdmission`] reports
    /// it. `overridable` is derived from
    /// [`Self::dispatch_pause_exempts_reviews`] — that flag is already
    /// `true` iff the active pause is operator-originated (see
    /// [`Self::pause_dispatch`]), so it doubles as the overridability
    /// signal without a separate stored origin field.
    fn dispatch_pause_snapshot(&self) -> DispatchPauseSnapshot {
        if !self.is_dispatch_paused() {
            return DispatchPauseSnapshot::default();
        }
        let overridable = self.dispatch_pause_exempts_reviews();
        DispatchPauseSnapshot {
            active: true,
            origin: Some(if overridable { "operator" } else { "breaker" }.to_string()),
            reason: self.dispatch_paused_reason(),
            paused_since_epoch_s: self.dispatch_paused_since_epoch_s(),
            overridable,
        }
    }

    /// Mark `execution_id` exempt from `drain_ready_queue`'s pause gate for
    /// exactly the next row-visit that consumes it. See the
    /// `dispatch_pause_bypass_execution_ids` field doc for the full
    /// single-shot contract.
    pub(super) fn mark_dispatch_pause_bypass(&self, execution_id: &str) {
        self.dispatch_pause_bypass_execution_ids
            .lock()
            .unwrap()
            .insert(execution_id.to_string());
    }

    /// Consume (and report whether it was present) the pause-bypass marker
    /// for `execution_id`. Called from `drain_ready_queue`'s pause gate
    /// (the normal single-shot consumption) and, belt-and-suspenders, from
    /// [`Self::dispatch_with_pause_bypass`] after its own drain pass
    /// returns, so a row the pass never even visited can't remain
    /// bypass-eligible for a later pass.
    pub(super) fn take_dispatch_pause_bypass(&self, execution_id: &str) -> bool {
        self.dispatch_pause_bypass_execution_ids
            .lock()
            .unwrap()
            .remove(execution_id)
    }

    pub(super) fn mark_requested_host(&self, execution_id: &str, host_id: String) {
        self.requested_host_ids
            .lock()
            .unwrap()
            .insert(execution_id.to_string(), host_id);
    }

    pub(super) fn take_requested_host(&self, execution_id: &str) -> Option<String> {
        self.requested_host_ids.lock().unwrap().remove(execution_id)
    }

    /// Mutating: admit `input.work_item_id` past an active, overridable
    /// (operator-origin) global dispatch pause for this one request, or
    /// refuse and explain why — using the exact same
    /// [`Self::evaluate_dispatch_admission`] the read-only preview RPC
    /// calls. Requires `input.entry_point` to be set.
    ///
    /// When no pause is active at all, this is indistinguishable from an
    /// ordinary `RequestExecution` — no synchronous wait, no override or
    /// refusal event recorded, per the design's "pause was lifted"
    /// handling. When a pause IS active and overridable, and every other
    /// constraint already clears, this creates/reuses the `ready` row
    /// exactly as the ordinary path does, marks it bypass-eligible for one
    /// `drain_ready_queue` pass, and synchronously awaits that pass so the
    /// caller learns the real outcome (dispatched, or refused with no
    /// residue) instead of "queued, maybe."
    pub async fn dispatch_with_pause_bypass(
        self: &Arc<Self>,
        input: RequestExecutionInput,
        live_states: Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
    ) -> Result<WorkExecution> {
        let entry_point = input
            .entry_point
            .context("bypass_dispatch_pause requires entry_point")?;
        let work_item_id = input.work_item_id.clone();
        let observed_pause_since_epoch_s = input.observed_pause_since_epoch_s;
        let admission = self.evaluate_dispatch_admission(&work_item_id).await?;

        match pause_bypass_decision(&admission, observed_pause_since_epoch_s) {
            Ok(true) => {}
            Ok(false) => {
                // Nothing to override: proceed exactly as an ordinary
                // explicit start would, with none of the bypass machinery
                // below.
                return self.request_execution_ordinary(input, live_states).await;
            }
            Err(reason) => {
                self.emit_pause_override_refused(&work_item_id, entry_point, &admission.pause, &reason)
                    .await;
                bail!("cannot force-start {work_item_id}: {reason}");
            }
        }

        // Every non-pause constraint clears, and the pause is a fresh,
        // overridable, operator-origin pause. Capture the pre-existing
        // non-terminal execution ids so we can tell "reused an execution
        // that predated this request" (leave it alone on refusal) from "we
        // created this row" (cancel it on refusal — no residue).
        let pre_existing: HashSet<String> = self
            .work_db
            .list_executions(Some(&work_item_id))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !e.status.is_terminal())
            .map(|e| e.id)
            .collect();

        let execution = self.request_execution_via_db(input, live_states)?;
        let was_new = !pre_existing.contains(&execution.id);

        if execution.status != ExecutionStatus::Ready {
            // Idempotent path: an already-live execution already owns the
            // dispatch slot for this work item — nothing to bypass.
            return Ok(execution);
        }

        self.mark_dispatch_pause_bypass(&execution.id);
        self.drain_ready_queue().await;
        // Belt-and-suspenders single-shot consumption — see the doc on
        // `take_dispatch_pause_bypass`.
        self.take_dispatch_pause_bypass(&execution.id);

        // `drain_ready_queue`'s serial decision section (claim vs. hold) is
        // synchronous and fully complete by the time the `.await` above
        // returns — every row it claimed in this pass has already taken its
        // `inflight_dispatches` reservation and had its slow
        // `schedule_execution` tail spawned off (see `dispatch_inflight`'s
        // module doc), so "was this row claimed" is knowable right now with
        // no waiting. Deliberately NOT waiting for that tail to finish: it
        // runs cube operations (repo ensure, workspace lease) that can take
        // up to their own multi-retry budget, and once force has gotten the
        // row *claimed* past the pause, whether the spawn itself then
        // succeeds or fails is an infrastructure question the engine's
        // ordinary retry/backoff machinery already owns — exactly as it
        // would for a claim an unpaused drain pass made. Re-checking DB
        // status first covers the (rare, fake-runner-speed) case where the
        // whole tail already finished inline before this line runs.
        let refreshed = self.work_db.get_execution(&execution.id).unwrap_or(execution);
        let claimed = refreshed.status != ExecutionStatus::Ready
            || self.inflight_dispatches.is_work_item_dispatching(&work_item_id);
        if claimed {
            self.dispatch_events
                .emit(
                    DispatchEvent::new(Stage::DispatchPauseOverride, DispatchOutcome::Ok, refreshed.id.clone())
                        .with_work_item(refreshed.work_item_id.clone())
                        .with_details(serde_json::json!({
                            "entry_point": entry_point_label(entry_point),
                            "pause_origin": admission.pause.origin,
                            "pause_reason": admission.pause.reason,
                            "paused_since_epoch_s": admission.pause.paused_since_epoch_s,
                        })),
                )
                .await;
            return Ok(refreshed);
        }

        // Never claimed at all -- a dynamic constraint (interactive cap, a
        // chain hold, ...) still held it once the pause was set aside.
        // Leave no residue for a row this request created.
        let reason = refreshed
            .dispatch_wait_reason
            .clone()
            .unwrap_or_else(|| "blocked after the pause was set aside (reason not recorded)".to_string());
        if was_new {
            let _ = self.work_db.cancel_execution_with(
                &refreshed.id,
                CancelExecutionOpts {
                    reason: Some(format!("pause-only forced dispatch refused: {reason}")),
                    queued_only: true,
                },
            );
        }
        self.emit_pause_override_refused(&work_item_id, entry_point, &admission.pause, &reason)
            .await;
        bail!("cannot force-start {work_item_id}: {reason}");
    }

    /// The plain (non-bypass) `request_execution` + `kick()` path, shared
    /// by [`Self::dispatch_with_pause_bypass`]'s "pause already lifted"
    /// case so it behaves identically to an ordinary `RequestExecution`.
    async fn request_execution_ordinary(
        self: &Arc<Self>,
        input: RequestExecutionInput,
        live_states: Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
    ) -> Result<WorkExecution> {
        let execution = self.request_execution_via_db(input, live_states)?;
        self.kick();
        Ok(execution)
    }

    pub(crate) fn request_execution_via_db(
        &self,
        input: RequestExecutionInput,
        live_states: Arc<crate::live_worker_state::LiveWorkerStateRegistry>,
    ) -> Result<WorkExecution> {
        if let Some(host_id) = input.requested_host_id.as_deref() {
            let live_execution = self
                .work_db
                .list_executions(Some(&input.work_item_id))?
                .into_iter()
                .find(|execution| !execution.status.is_terminal() && live_states.is_run_live(&execution.id));
            if let Some(execution) = live_execution {
                bail!(
                    "{} already has a live execution {}; --host cannot move a running dispatch",
                    input.work_item_id,
                    execution.id
                );
            }
            self.validate_requested_host(&input.work_item_id, host_id)?;
        }
        let requested_host_id = input.requested_host_id.clone();
        let execution = self
            .work_db
            .request_execution_with_live_check(input, move |run_id| live_states.is_run_live(run_id))?;
        if let Some(host_id) = requested_host_id {
            if execution.status != ExecutionStatus::Ready {
                bail!(
                    "{} already has a live execution {}; --host cannot move a running dispatch",
                    execution.work_item_id,
                    execution.id
                );
            }
            // This request-only routing constraint never touches the durable
            // pin escape hatch persisted on the execution.
            self.mark_requested_host(&execution.id, host_id);
        }
        Ok(execution)
    }

    async fn emit_pause_override_refused(
        &self,
        work_item_id: &str,
        entry_point: DispatchAdmissionEntryPoint,
        pause: &DispatchPauseSnapshot,
        reason: &str,
    ) {
        self.dispatch_events
            .emit(
                DispatchEvent::new(
                    Stage::DispatchPauseOverrideRefused,
                    DispatchOutcome::Skipped,
                    work_item_id,
                )
                .with_work_item(work_item_id)
                .with_details(serde_json::json!({
                    "entry_point": entry_point_label(entry_point),
                    "reason": reason,
                    "pause_active": pause.active,
                    "pause_origin": pause.origin,
                    "pause_reason": pause.reason,
                    "paused_since_epoch_s": pause.paused_since_epoch_s,
                })),
            )
            .await;
    }
}
