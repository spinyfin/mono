//! Shared execution-admission evaluation for explicit starts.
//!
//! One reason-producing function answers "would this work item dispatch
//! right now under the evaluated intent, and if not, why?" The read-only
//! `EvaluateExecutionAdmission` RPC and the mutating `RequestExecution`
//! path both call [`evaluate_execution_admission`] so the macOS confirmation
//! dialog cannot drift from the engine's real refusal.
//!
//! `bypass_dispatch_pause` overrides **only** an active operator global
//! dispatch pause. It does not grow the pool (that remains the separate
//! `RequestExecutionInput::force` pool-growth bit) and does not clear
//! concurrency caps, dependencies, ineligible status, or a breaker-
//! originated pause.

use anyhow::Result;
use boss_protocol::{
    DispatchPauseSnapshot, ExecutionAdmissionBlocker, ExecutionAdmissionEvaluation, TaskStatus, WorkItem,
};

use crate::coordinator::{DispatchPauseOrigin, ExecutionCoordinator};
use crate::work::WorkDb;

/// Intent evaluated by [`evaluate_execution_admission`].
#[derive(Debug, Clone, Copy, Default)]
pub struct AdmissionIntent {
    /// When true, an operator-originated global pause is not a hard blocker
    /// (it becomes an overridable condition that may yield
    /// `would_override_pause`).
    pub bypass_dispatch_pause: bool,
    /// Pause generation the client observed. When set and the live pause
    /// generation differs, evaluation reports a stale-confirmation blocker.
    pub observed_pause_generation: Option<u64>,
}

/// Snapshot of coordinator state the evaluator needs.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct AdmissionRuntimeSnapshot {
    pub dispatch_paused: bool,
    pub pause_origin: Option<DispatchPauseOrigin>,
    pub pause_reason: Option<String>,
    pub paused_since_epoch_s: Option<u64>,
    pub reviews_exempt: bool,
    pub preflight_block_reason: Option<String>,
    pub interactive_busy: usize,
    pub interactive_cap: usize,
}

impl AdmissionRuntimeSnapshot {
    pub async fn from_coordinator(coordinator: &ExecutionCoordinator) -> Self {
        let dispatch_paused = coordinator.is_dispatch_paused();
        let reviews_exempt = coordinator.dispatch_pause_exempts_reviews();
        let pause_origin = if dispatch_paused {
            Some(if reviews_exempt {
                DispatchPauseOrigin::Operator
            } else {
                DispatchPauseOrigin::Breaker
            })
        } else {
            None
        };
        Self {
            dispatch_paused,
            pause_origin,
            pause_reason: coordinator.dispatch_paused_reason(),
            paused_since_epoch_s: coordinator.dispatch_paused_since_epoch_s(),
            reviews_exempt,
            preflight_block_reason: coordinator.dispatch_preflight_block_reason(),
            interactive_busy: coordinator.worker_pool().busy_count().await,
            interactive_cap: coordinator.max_concurrent_interactive_workers(),
        }
    }

    fn pause_snapshot(&self) -> DispatchPauseSnapshot {
        DispatchPauseSnapshot::builder()
            .paused(self.dispatch_paused)
            .maybe_origin(self.pause_origin.map(|o| o.as_metadata_str().to_owned()))
            .maybe_reason(self.pause_reason.clone())
            .maybe_paused_since_epoch_s(self.paused_since_epoch_s)
            .reviews_exempt(self.reviews_exempt)
            .build()
    }
}

/// Stable reason codes (also used in dispatch-event details).
pub mod reason_code {
    pub const DISPATCH_PAUSED: &str = "dispatch_paused";
    pub const STALE_PAUSE_CONFIRMATION: &str = "stale_pause_confirmation";
    pub const INTERACTIVE_CONCURRENCY_CAP: &str = "interactive_concurrency_cap";
    pub const UNMET_DEPENDENCIES: &str = "unmet_dependencies";
    pub const INELIGIBLE_STATUS: &str = "ineligible_status";
    pub const HUMAN_DRIVEN: &str = "human_driven";
    pub const LIVE_EXECUTION: &str = "live_execution";
    pub const UNRESOLVED_REPO: &str = "unresolved_repo";
    pub const STARTUP_PREFLIGHT: &str = "startup_preflight";
    pub const WORK_ITEM_NOT_FOUND: &str = "work_item_not_found";
}

fn blocker(code: &str, message: impl Into<String>, force_overridable: bool) -> ExecutionAdmissionBlocker {
    ExecutionAdmissionBlocker::builder()
        .code(code)
        .message(message)
        .force_overridable(force_overridable)
        .build()
}

/// Shared admission evaluation. See module docs.
pub fn evaluate_execution_admission(
    work_db: &WorkDb,
    work_item_id: &str,
    intent: AdmissionIntent,
    runtime: &AdmissionRuntimeSnapshot,
) -> Result<ExecutionAdmissionEvaluation> {
    let pause = runtime.pause_snapshot();
    let mut blockers: Vec<ExecutionAdmissionBlocker> = Vec::new();

    let resolved_id = {
        let conn = work_db.connect()?;
        crate::work::resolve_friendly_work_item_id(&conn, work_item_id)?.unwrap_or_else(|| work_item_id.to_owned())
    };

    let item = match work_db.get_work_item(&resolved_id) {
        Ok(item) => item,
        Err(err) => {
            blockers.push(blocker(
                reason_code::WORK_ITEM_NOT_FOUND,
                format!("cannot start {work_item_id}: {err}"),
                false,
            ));
            return Ok(build_result(resolved_id, false, pause, false, blockers, false));
        }
    };

    match &item {
        WorkItem::Task(task) | WorkItem::Chore(task) => {
            if task.status.is_terminal() {
                let reason_suffix = task
                    .archived_reason
                    .as_ref()
                    .map(|r| format!(" — {r}"))
                    .unwrap_or_default();
                blockers.push(blocker(
                    reason_code::INELIGIBLE_STATUS,
                    format!(
                        "cannot start {}: item is `{}`{reason_suffix} — terminal work items cannot be dispatched",
                        task.id, task.status
                    ),
                    false,
                ));
            } else if task.human_driven {
                blockers.push(blocker(
                    reason_code::HUMAN_DRIVEN,
                    format!(
                        "cannot start {}: item is human-driven — no agent worker will run",
                        task.id
                    ),
                    false,
                ));
            } else if task.status == TaskStatus::Blocked {
                // Stale dependency blocks are cleared by request_execution when
                // prereqs are satisfied; other blocked reasons remain ineligible.
                let reason = task.blocked_reason.as_deref().unwrap_or("unknown");
                if reason != "dependency" {
                    blockers.push(blocker(
                        reason_code::INELIGIBLE_STATUS,
                        format!("cannot start {}: item is blocked ({reason})", task.id),
                        false,
                    ));
                }
            }
        }
        WorkItem::Product(_) | WorkItem::Project(_) => {
            blockers.push(blocker(
                reason_code::INELIGIBLE_STATUS,
                format!("cannot start {resolved_id}: only tasks/chores can be dispatched"),
                false,
            ));
        }
    }

    let gating = work_db.gating_prereqs_for(&resolved_id).unwrap_or_default();
    if !gating.is_empty() {
        let names = gating.join(", ");
        blockers.push(blocker(
            reason_code::UNMET_DEPENDENCIES,
            format!("cannot start {resolved_id}: gated by [{names}]"),
            false,
        ));
    }

    if matches!(item, WorkItem::Task(_) | WorkItem::Chore(_))
        && work_db.resolve_repo_for_task(&resolved_id).ok().flatten().is_none()
    {
        blockers.push(blocker(
            reason_code::UNRESOLVED_REPO,
            format!(
                "cannot start {resolved_id}: no repository is configured \
                 (set a product default repo or a per-task override)"
            ),
            false,
        ));
    }

    if let Ok(Some(existing)) = work_db.latest_execution_for_work_item(&resolved_id)
        && existing.status.is_live()
    {
        blockers.push(blocker(
            reason_code::LIVE_EXECUTION,
            format!(
                "cannot start {resolved_id}: execution {} is already `{}`",
                existing.id, existing.status
            ),
            false,
        ));
    }

    if let Some(reason) = &runtime.preflight_block_reason {
        blockers.push(blocker(
            reason_code::STARTUP_PREFLIGHT,
            format!("local dispatch is unavailable: {reason}"),
            false,
        ));
    }

    let pause_overridable =
        runtime.dispatch_paused && matches!(runtime.pause_origin, Some(DispatchPauseOrigin::Operator));

    if let Some(observed) = intent.observed_pause_generation
        && runtime.dispatch_paused
    {
        let live = runtime.paused_since_epoch_s.unwrap_or(0);
        if live != observed {
            let new_reason = runtime.pause_reason.as_deref().unwrap_or("(no reason recorded)");
            blockers.push(blocker(
                reason_code::STALE_PAUSE_CONFIRMATION,
                format!(
                    "dispatch pause changed since confirmation (now: {new_reason}); \
                     re-evaluate and confirm again"
                ),
                false,
            ));
        }
    }

    let mut would_override_pause = false;
    if runtime.dispatch_paused {
        let reason = runtime.pause_reason.as_deref().unwrap_or("(no reason recorded)");
        match runtime.pause_origin {
            Some(DispatchPauseOrigin::Operator) if intent.bypass_dispatch_pause => {
                let generation_ok = match intent.observed_pause_generation {
                    None => true,
                    Some(obs) => runtime.paused_since_epoch_s == Some(obs),
                };
                if generation_ok {
                    would_override_pause = true;
                }
            }
            Some(DispatchPauseOrigin::Operator) => {
                blockers.push(blocker(
                    reason_code::DISPATCH_PAUSED,
                    format!("dispatch is paused: {reason}"),
                    true,
                ));
            }
            Some(DispatchPauseOrigin::Breaker) | None => {
                blockers.push(blocker(
                    reason_code::DISPATCH_PAUSED,
                    format!(
                        "dispatch is paused by the spawn-capability circuit breaker: {reason} \
                         (not overridable with --force)"
                    ),
                    false,
                ));
            }
        }
    }

    // Cap is a hard refuse only when we would immediately dispatch through a
    // pause override. Ordinary starts may still queue under the cap.
    if would_override_pause && runtime.interactive_busy >= runtime.interactive_cap {
        blockers.push(blocker(
            reason_code::INTERACTIVE_CONCURRENCY_CAP,
            format!(
                "interactive concurrency cap reached ({}/{} workers live) — \
                 force does not grow the pool or queue past the cap while dispatch is paused",
                runtime.interactive_busy, runtime.interactive_cap
            ),
            false,
        ));
        would_override_pause = false;
    }

    let hard_empty = blockers
        .iter()
        .all(|b| b.force_overridable && intent.bypass_dispatch_pause);
    let would_admit = hard_empty;
    if !would_admit {
        would_override_pause = false;
    }

    Ok(build_result(
        resolved_id,
        would_admit,
        pause,
        pause_overridable,
        blockers,
        would_override_pause,
    ))
}

fn build_result(
    work_item_id: String,
    would_admit: bool,
    pause: DispatchPauseSnapshot,
    pause_overridable: bool,
    blockers: Vec<ExecutionAdmissionBlocker>,
    would_override_pause: bool,
) -> ExecutionAdmissionEvaluation {
    ExecutionAdmissionEvaluation::builder()
        .work_item_id(work_item_id)
        .would_admit(would_admit)
        .pause(pause)
        .pause_overridable(pause_overridable)
        .blockers(blockers)
        .would_override_pause(would_override_pause)
        .build()
}

/// Format blockers into a single operator-facing refusal message.
pub fn format_refusal_message(evaluation: &ExecutionAdmissionEvaluation) -> String {
    let msgs: Vec<&str> = evaluation
        .blockers
        .iter()
        .filter(|b| !(b.force_overridable && evaluation.would_override_pause))
        .map(|b| b.message.as_str())
        .collect();
    if msgs.is_empty() {
        format!("cannot start {}: admission refused", evaluation.work_item_id)
    } else {
        msgs.join("; ")
    }
}

/// Join blocker codes for structured events.
pub fn blocker_codes(evaluation: &ExecutionAdmissionEvaluation) -> Vec<String> {
    evaluation.blockers.iter().map(|b| b.code.clone()).collect()
}
