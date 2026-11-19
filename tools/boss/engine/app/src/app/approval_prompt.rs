//! A worker launched to never ask, left waiting on an approval prompt.
//!
//! The capability-restricted kinds (answer agent, review guide, progression)
//! run deny-by-default: a call outside their allowlist is refused and the
//! worker carries on. Nobody watches their panes, so a prompt that opens there
//! anyway is never answered. Left unhandled, such a run holds its pool slot
//! until some unrelated timeout, and enough of them exhaust the pool.
//!
//! This module turns that state into a recorded failure. When a restricted
//! worker's driver reports an approval prompt
//! (`AgentDriver::approval_prompt_reported`) and the prompt is still
//! outstanding after [`UNANSWERED_APPROVAL_PROMPT_GRACE`], the run is failed
//! with the reason and its pane and slot are released. An unrelated later
//! event (a Grok `task_complete` Notification, for example) does not clear
//! the outstanding prompt; only evidence that the dialog closed does.
//!
//! # Why only the restricted kinds
//!
//! An ordinary worker is launched to approve everything, and its driver may
//! raise the same signal and answer it itself: Grok does so about 20ms later
//! for a command it cannot check against its deny rules, and the tool then
//! runs for as long as it runs with no further event. The signal says nothing
//! about such a pane being stuck. For a kind launched without always-approve
//! there is nothing to answer the prompt, so the signal followed by an
//! unclosed dialog is the stuck state itself.
//!
//! # What this does not cover
//!
//! The watch is a timer started by the prompt's own event, held in memory. An
//! engine restart during the grace window loses it. A progression run is then
//! bounded by `progression.run_timeout` in the stale-worker sweep, which
//! evaluates that timeout even while the slot is still `Spawning` (adopted
//! panes start there, and Grok never reports `AwaitingInput`).

use boss_dispatch_events::{DispatchEvent, Outcome, Stage};
use boss_engine_runtime::events_socket::IncomingHookEvent;
use boss_protocol::WorkerEvent;

use super::*;

/// How long an approval prompt reported by a restricted worker may stay
/// outstanding before the run is failed.
///
/// The prompt cannot resolve without a keypress, so this is not a wait for
/// something expected to happen. It is the margin against acting on a prompt
/// that was in fact answered. The only automatic answer measured takes 0.02s
/// (Grok under always-approve, 2026-11-18), and a restricted session has none
/// at all. Two minutes is several thousand times the first and costs one
/// slot two minutes instead of a run timeout's worth.
///
/// When it fires the execution is recorded failed with the reason below (basis
/// `blocking_dialog_on_screen`; a progression run on its own row only), its
/// pane is released and a `stale_worker_reconcile` dispatch event carries
/// `unanswered_approval_prompt`.
pub(super) const UNANSWERED_APPROVAL_PROMPT_GRACE: Duration = Duration::from_secs(120);

/// A deny-by-default run's last reported approval prompt, still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OutstandingApprovalPrompt {
    prompt_ingress_seq: Option<u64>,
}

impl ServerState {
    /// Start the unanswered-prompt watch for `incoming` when it is an approval
    /// prompt from a worker launched to never ask. For any other event, clear
    /// an outstanding prompt only when the event is evidence the dialog closed.
    ///
    /// The worker-event fan-out calls this right after the live-state
    /// transition, so a prompt is recorded as the slot the prompt left it.
    pub(super) fn watch_approval_prompt(self: &Arc<Self>, incoming: &IncomingHookEvent) {
        let Some((run_id, mark)) = self.approval_prompt_to_watch(incoming) else {
            return;
        };
        tracing::warn!(
            run_id,
            grace_secs = UNANSWERED_APPROVAL_PROMPT_GRACE.as_secs(),
            "a worker launched deny-by-default reported an interactive approval prompt; \
             the run is failed if that prompt is still outstanding after the grace period",
        );
        let state = self.clone();
        boss_task_scope::spawn(async move {
            tokio::time::sleep(UNANSWERED_APPROVAL_PROMPT_GRACE).await;
            state
                .fail_if_approval_prompt_unanswered(&run_id, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
                .await;
        });
    }

    /// The run and outstanding prompt to watch for `incoming`, or `None` when
    /// it is not an approval prompt this module acts on. Non-prompt events
    /// that close a dialog clear any outstanding entry for that run.
    pub(super) fn approval_prompt_to_watch(
        &self,
        incoming: &IncomingHookEvent,
    ) -> Option<(String, OutstandingApprovalPrompt)> {
        if !incoming.reports_approval_prompt() {
            self.clear_outstanding_approval_if_dialog_closed(incoming);
            return None;
        }
        let run_id = incoming.run_id.as_deref()?;
        if !self.launched_deny_by_default(run_id) {
            return None;
        }
        // A newer event already applied means the prompt was answered before
        // this one was even dispatched.
        if let Some(prompt_seq) = incoming.connection.ingress_seq
            && let Some(slot_id) = self.worker_registry.slot_for_run(run_id)
            && self
                .live_worker_states
                .last_applied_ingress_seq(slot_id)
                .is_some_and(|applied| applied > prompt_seq)
        {
            return None;
        }
        let mark = OutstandingApprovalPrompt {
            prompt_ingress_seq: incoming.connection.ingress_seq,
        };
        self.record_outstanding_approval(run_id, mark.clone());
        Some((run_id.to_owned(), mark))
    }

    /// Whether `run_id` is a live execution of a kind the engine launches
    /// with a forced permission mode, and so without always-approve.
    fn launched_deny_by_default(&self, run_id: &str) -> bool {
        self.work_db.get_execution(run_id).is_ok_and(|execution| {
            !execution.status.is_terminal()
                && boss_execution_contracts::worker_kind_for_execution(&execution.kind)
                    .forced_permission_mode()
                    .is_some()
        })
    }

    fn record_outstanding_approval(&self, run_id: &str, mark: OutstandingApprovalPrompt) {
        self.outstanding_approval_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(run_id.to_owned(), mark);
    }

    fn take_outstanding_approval(&self, run_id: &str) -> Option<OutstandingApprovalPrompt> {
        self.outstanding_approval_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(run_id)
    }

    fn outstanding_approval(&self, run_id: &str) -> Option<OutstandingApprovalPrompt> {
        self.outstanding_approval_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(run_id)
            .cloned()
    }

    /// Evidence the approval dialog closed: a new tool attempt, a finished
    /// tool, a turn or session end. A Notification (including Grok's
    /// `task_complete`) is not evidence.
    fn clear_outstanding_approval_if_dialog_closed(&self, incoming: &IncomingHookEvent) {
        let Some(run_id) = incoming.run_id.as_deref() else {
            return;
        };
        if !dialog_closed_by(&incoming.event) {
            return;
        }
        let _ = self.take_outstanding_approval(run_id);
    }

    /// Fail `run_id` if the approval prompt that produced `mark` is still
    /// outstanding. Returns whether the run was failed. The pane and slot are
    /// released even when recording the failure write itself fails.
    pub(super) async fn fail_if_approval_prompt_unanswered(
        &self,
        run_id: &str,
        mark: &OutstandingApprovalPrompt,
        waited: Duration,
    ) -> bool {
        if self.outstanding_approval(run_id).as_ref() != Some(mark) || !self.launched_deny_by_default(run_id) {
            return false;
        }
        let _ = self.take_outstanding_approval(run_id);
        let driver = self
            .pane_input_driver(run_id)
            .map(|driver| driver.descriptor().name.to_owned())
            .unwrap_or_else(|_| "unknown".to_owned());
        let detail = format!(
            "UnansweredApprovalPrompt: the {driver} worker opened an interactive approval prompt and \
             nothing answered it for {}s. This kind of run is launched deny-by-default and has nobody \
             to approve a call, so the prompt means its permission mode did not take effect. The run \
             was failed and its pane released instead of holding the slot.",
            waited.as_secs()
        );
        tracing::error!(run_id, driver, waited_secs = waited.as_secs(), "{detail}");
        let write = self.completion_handler.fail_blocking_dialog(run_id, &detail).await;
        if let Err(error) = &write {
            tracing::error!(
                run_id,
                %error,
                "could not record the unanswered-approval-prompt failure; releasing the pane and slot regardless \
                 (finalize may already have torn the pane down on a progression path)"
            );
        }
        let release = self.release_worker_pane(run_id).await;
        // A failed failure-write can also make tmux teardown's durable
        // bookkeeping fail, in which case `release_worker_pane` returns
        // without dropping the in-memory slot. Drop it here so the pool
        // is not held by a run we have already decided to fail.
        if self.worker_registry.slot_for_run(run_id).is_some() {
            tracing::error!(
                run_id,
                ?release,
                "unanswered-approval-prompt: pane teardown did not drop the slot; dropping it anyway"
            );
            self.worker_registry.take_slot_for_run(run_id);
            self.live_worker_states.release_slot_for_run(run_id);
            self.live_worker_states.release_persona_for_run(run_id);
        }
        let mut details = serde_json::json!({
            "unanswered_approval_prompt": true,
            "driver": driver,
            "waited_secs": waited.as_secs(),
            "pane_release": format!("{release:?}"),
        });
        if let Err(error) = &write {
            details["failure_write_error"] = serde_json::Value::String(error.to_string());
        }
        let mut event = DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Ok, run_id).with_details(details);
        if let Ok(execution) = self.work_db.get_execution(run_id) {
            event = event.with_work_item(execution.work_item_id);
        }
        self.dispatch_events.emit(event).await;
        true
    }
}

fn dialog_closed_by(event: &WorkerEvent) -> bool {
    matches!(
        event,
        WorkerEvent::PreToolUse { .. }
            | WorkerEvent::PostToolUse { .. }
            | WorkerEvent::Stop { .. }
            | WorkerEvent::SessionEnd { .. }
    )
}
