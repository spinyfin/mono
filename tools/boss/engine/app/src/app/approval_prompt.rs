//! A worker launched to never ask, left waiting on an approval prompt.
//!
//! The capability-restricted kinds (answer agent, review guide, progression)
//! run deny-by-default: a call outside their allowlist is refused and the
//! worker carries on. Nobody watches their panes, so a prompt that opens there
//! anyway is never answered. The run then looks alive and busy, a tool call
//! in flight, for as long as its pane exists, and holds its pool slot the
//! whole time. Four Grok progression runs did exactly that for over 80
//! minutes each and starved the progression pool.
//!
//! This module turns that state into a recorded failure. When a restricted
//! worker's driver reports an approval prompt
//! (`AgentDriver::approval_prompt_reported`) and nothing at all follows it for
//! [`UNANSWERED_APPROVAL_PROMPT_GRACE`], the run is failed with the reason and
//! its pane and slot are released.
//!
//! # Why only the restricted kinds
//!
//! An ordinary worker is launched to approve everything, and its driver may
//! raise the same signal and answer it itself: Grok does so about 20ms later
//! for a command it cannot check against its deny rules, and the tool then
//! runs for as long as it runs with no further event. The signal says nothing
//! about such a pane being stuck. For a kind launched without always-approve
//! there is nothing to answer the prompt, so the signal followed by silence
//! is the stuck state itself.
//!
//! # What this does not cover
//!
//! The watch is a timer started by the prompt's own event, held in memory. An
//! engine restart during the grace window loses it. A progression run is then
//! bounded by `progression.run_timeout` in the stale-worker sweep, as before.

use boss_dispatch_events::{DispatchEvent, Outcome, Stage};
use boss_engine_runtime::events_socket::IncomingHookEvent;

use super::*;

/// How long an approval prompt reported by a restricted worker may go with no
/// further event before the run is failed.
///
/// The prompt cannot resolve without a keypress, so this is not a wait for
/// something expected to happen. It is the margin against acting on a prompt
/// that was in fact answered. The only automatic answer measured takes 0.02s
/// (Grok under always-approve, 2026-11-18), and a restricted session has none
/// at all: the prompt held for the whole of every probe and for 80 minutes in
/// production. Two minutes is several thousand times the first and costs one
/// slot two minutes instead of a run timeout's worth.
///
/// When it fires the execution is recorded failed with the reason below (basis
/// `blocking_dialog_on_screen`; a progression run on its own row only), its
/// pane is released and a `stale_worker_reconcile` dispatch event carries
/// `unanswered_approval_prompt`.
pub(super) const UNANSWERED_APPROVAL_PROMPT_GRACE: Duration = Duration::from_secs(120);

/// The state of a worker's slot the moment after its approval prompt was
/// applied. Any later event from the worker changes it, so an unchanged mark
/// means nothing has happened since the prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ApprovalPromptMark {
    last_applied_ingress_seq: Option<u64>,
    last_event_at: Option<String>,
}

impl ServerState {
    /// Start the unanswered-prompt watch for `incoming` when it is an approval
    /// prompt from a worker launched to never ask. A no-op for every other
    /// event, which is nearly all of them.
    ///
    /// The worker-event fan-out calls this right after the live-state
    /// transition, so the mark it takes is the slot as the prompt left it.
    pub(super) fn watch_approval_prompt(self: &Arc<Self>, incoming: &IncomingHookEvent) {
        let Some((run_id, mark)) = self.approval_prompt_to_watch(incoming) else {
            return;
        };
        tracing::warn!(
            run_id,
            grace_secs = UNANSWERED_APPROVAL_PROMPT_GRACE.as_secs(),
            "a worker launched deny-by-default reported an interactive approval prompt; \
             the run is failed if nothing follows it within the grace period",
        );
        let state = self.clone();
        boss_task_scope::spawn(async move {
            tokio::time::sleep(UNANSWERED_APPROVAL_PROMPT_GRACE).await;
            state
                .fail_if_approval_prompt_unanswered(&run_id, &mark, UNANSWERED_APPROVAL_PROMPT_GRACE)
                .await;
        });
    }

    /// The run and slot mark to watch for `incoming`, or `None` when it is not
    /// an approval prompt this module acts on.
    pub(super) fn approval_prompt_to_watch(
        &self,
        incoming: &IncomingHookEvent,
    ) -> Option<(String, ApprovalPromptMark)> {
        if !incoming.reports_approval_prompt() {
            return None;
        }
        let run_id = incoming.run_id.as_deref()?;
        if !self.launched_deny_by_default(run_id) {
            return None;
        }
        let mark = self.approval_prompt_mark(run_id)?;
        // A newer event already applied means the prompt was answered before
        // this one was even dispatched.
        if let Some(prompt_seq) = incoming.connection.ingress_seq
            && mark
                .last_applied_ingress_seq
                .is_some_and(|applied| applied > prompt_seq)
        {
            return None;
        }
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

    fn approval_prompt_mark(&self, run_id: &str) -> Option<ApprovalPromptMark> {
        let slot_id = self.worker_registry.slot_for_run(run_id)?;
        Some(ApprovalPromptMark {
            last_applied_ingress_seq: self.live_worker_states.last_applied_ingress_seq(slot_id),
            last_event_at: self.live_worker_states.last_event_at_for_run(run_id),
        })
    }

    /// Fail `run_id` if its worker has reported nothing since the approval
    /// prompt that produced `mark`. Returns whether the run was failed.
    pub(super) async fn fail_if_approval_prompt_unanswered(
        &self,
        run_id: &str,
        mark: &ApprovalPromptMark,
        waited: Duration,
    ) -> bool {
        if self.approval_prompt_mark(run_id).as_ref() != Some(mark) || !self.launched_deny_by_default(run_id) {
            return false;
        }
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
        if let Err(error) = self.completion_handler.fail_blocking_dialog(run_id, &detail).await {
            tracing::error!(run_id, %error, "could not record the unanswered-approval-prompt failure; pane left in place");
            return false;
        }
        let release = self.release_worker_pane(run_id).await;
        let mut event =
            DispatchEvent::new(Stage::StaleWorkerReconcile, Outcome::Ok, run_id).with_details(serde_json::json!({
                "unanswered_approval_prompt": true,
                "driver": driver,
                "waited_secs": waited.as_secs(),
                "pane_release": format!("{release:?}"),
            }));
        if let Ok(execution) = self.work_db.get_execution(run_id) {
            event = event.with_work_item(execution.work_item_id);
        }
        self.dispatch_events.emit(event).await;
        true
    }
}
