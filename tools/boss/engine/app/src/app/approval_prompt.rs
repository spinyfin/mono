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
//! # Ordering
//!
//! Each hook connection is dispatched on its own task, so events reach this
//! module out of their ingress order. The watch therefore keeps, per run, the
//! ingress sequence of the outstanding prompt and of the newest
//! dialog-closing event, independently of the live-state registry's generic
//! last-applied sequence. Only a closing event newer than the prompt clears
//! it, and only a closing event newer than a prompt suppresses that prompt
//! when it arrives late. An event without a sequence cannot be ordered and is
//! treated as applying.
//!
//! # Retrying an incomplete failure
//!
//! Failing the run needs a durable write and a verified pane teardown, in
//! that order: the pane is not touched until the failure is recorded. When
//! either step does not complete, the pane may still be alive and the pool
//! claim is still held, so the watch keeps its record and tries again, backing
//! off from [`INCOMPLETE_FAILURE_RETRY_DELAY`] to
//! [`MAX_INCOMPLETE_FAILURE_RETRY_DELAY`], rather than dropping tracking. Once
//! the failure is recorded the teardown is owed: a dialog-closing event that
//! arrives afterwards does not cancel it.
//!
//! # Per-run state
//!
//! The first dialog-closing event of a run reads its execution row once and
//! caches whether the run is of a deny-by-default kind, so later tool and
//! turn events from any worker are answered from memory. The entry is dropped
//! when the run's pane is released, and when an unanswered-prompt failure
//! settles. A terminal run never gets a new entry.
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
//! bounded by `progression.run_timeout` in the progression deadline sweep, which
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

/// How long to wait before the first retry when failing a run did not
/// complete (the durable write or the verified pane teardown failed). Each
/// later retry waits twice as long as the one before, up to
/// [`MAX_INCOMPLETE_FAILURE_RETRY_DELAY`].
pub(super) const INCOMPLETE_FAILURE_RETRY_DELAY: Duration = Duration::from_secs(30);

/// The longest wait between retries of an incomplete failure. A failure that
/// stays incomplete (tmux evidence unavailable, say) is still retried, but no
/// more often than this.
pub(super) const MAX_INCOMPLETE_FAILURE_RETRY_DELAY: Duration = Duration::from_secs(600);

/// The wait before the retry that follows one that waited `previous`.
pub(super) fn next_incomplete_failure_retry_delay(previous: Duration) -> Duration {
    previous.saturating_mul(2).min(MAX_INCOMPLETE_FAILURE_RETRY_DELAY)
}

/// A deny-by-default run's last reported approval prompt, still open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OutstandingApprovalPrompt {
    prompt_ingress_seq: Option<u64>,
}

/// What the watch knows about one live run.
#[derive(Debug)]
pub(super) struct RunApprovalWatch {
    /// Whether the run is of a kind launched deny-by-default, read from its
    /// execution row once. A run of any other kind keeps only this flag.
    deny_by_default: bool,
    /// Ingress sequence of the newest dialog-closing event seen for the run,
    /// whether or not live state applied it.
    latest_close_seq: Option<u64>,
    outstanding: Option<OutstandingApprovalPrompt>,
}

impl RunApprovalWatch {
    fn new(deny_by_default: bool) -> Self {
        Self {
            deny_by_default,
            latest_close_seq: None,
            outstanding: None,
        }
    }
}

/// What earlier attempts at failing one run have already done. Carried from
/// one retry to the next by the watch's own task.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct UnansweredPromptProgress {
    /// The durable failure is recorded, so the pane teardown is owed whatever
    /// the outstanding prompt now reads.
    failure_recorded: bool,
    /// An incomplete attempt was already reported as a dispatch event; later
    /// incomplete attempts only log.
    incomplete_reported: bool,
}

/// Result of [`ServerState::fail_if_approval_prompt_unanswered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnansweredPromptOutcome {
    /// The prompt was answered, superseded or already handled; nothing done.
    NotApplicable,
    /// The run is recorded failed and its pane and slot are released.
    Failed,
    /// The failure write or the verified teardown did not complete. The watch
    /// is kept so a later attempt repeats both.
    Incomplete,
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
            let mut waited = UNANSWERED_APPROVAL_PROMPT_GRACE;
            let mut progress = UnansweredPromptProgress::default();
            let mut retry_delay = INCOMPLETE_FAILURE_RETRY_DELAY;
            tokio::time::sleep(waited).await;
            loop {
                let outcome = state
                    .fail_if_approval_prompt_unanswered(&run_id, &mark, waited, &mut progress)
                    .await;
                if outcome != UnansweredPromptOutcome::Incomplete {
                    break;
                }
                tokio::time::sleep(retry_delay).await;
                waited += retry_delay;
                retry_delay = next_incomplete_failure_retry_delay(retry_delay);
            }
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
            self.note_dialog_closing_event(incoming);
            return None;
        }
        let run_id = incoming.run_id.as_deref()?;
        if !self.launched_deny_by_default(run_id) {
            return None;
        }
        let mark = OutstandingApprovalPrompt {
            prompt_ingress_seq: incoming.connection.ingress_seq,
        };
        let mut watches = self.lock_approval_watches();
        let watch = watches
            .entry(run_id.to_owned())
            .or_insert_with(|| RunApprovalWatch::new(true));
        // A closing event newer than this prompt was dispatched first: the
        // dialog this prompt opened was already answered.
        if let (Some(prompt_seq), Some(close_seq)) = (mark.prompt_ingress_seq, watch.latest_close_seq)
            && close_seq > prompt_seq
        {
            return None;
        }
        // A newer prompt already outstanding supersedes this older one.
        if let (Some(existing), Some(prompt_seq)) = (
            watch.outstanding.as_ref().and_then(|open| open.prompt_ingress_seq),
            mark.prompt_ingress_seq,
        ) && existing > prompt_seq
        {
            return None;
        }
        watch.outstanding = Some(mark.clone());
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

    /// Whether `run_id` is of a kind the engine launches deny-by-default,
    /// whatever its current status.
    fn is_deny_by_default_kind(&self, run_id: &str) -> bool {
        self.work_db.get_execution(run_id).is_ok_and(|execution| {
            boss_execution_contracts::worker_kind_for_execution(&execution.kind)
                .forced_permission_mode()
                .is_some()
        })
    }

    /// Whether `run_id` is a live run of a deny-by-default kind, from the
    /// cached watch entry when there is one. Otherwise the execution row is
    /// read once and the answer cached for the run's lifetime; a terminal or
    /// unknown run is not cached, so a late event cannot leave an entry behind
    /// a released run.
    fn watched_kind_cached(&self, run_id: &str) -> bool {
        if let Some(watch) = self.lock_approval_watches().get(run_id) {
            return watch.deny_by_default;
        }
        let Ok(execution) = self.work_db.get_execution(run_id) else {
            return false;
        };
        if execution.status.is_terminal() {
            return false;
        }
        let deny_by_default = boss_execution_contracts::worker_kind_for_execution(&execution.kind)
            .forced_permission_mode()
            .is_some();
        self.lock_approval_watches()
            .entry(run_id.to_owned())
            .or_insert_with(|| RunApprovalWatch::new(deny_by_default))
            .deny_by_default
    }

    /// Drop everything the watch holds for `run_id`. Called when the run's
    /// pane is released and when its unanswered-prompt failure settles.
    pub(super) fn forget_approval_watch(&self, run_id: &str) {
        self.lock_approval_watches().remove(run_id);
    }

    #[cfg(test)]
    pub(super) fn has_approval_watch(&self, run_id: &str) -> bool {
        self.lock_approval_watches().contains_key(run_id)
    }

    fn lock_approval_watches(&self) -> std::sync::MutexGuard<'_, HashMap<String, RunApprovalWatch>> {
        self.outstanding_approval_prompts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn outstanding_approval(&self, run_id: &str) -> Option<OutstandingApprovalPrompt> {
        self.lock_approval_watches()
            .get(run_id)
            .and_then(|watch| watch.outstanding.clone())
    }

    /// Record an event that is evidence the approval dialog closed: a new tool
    /// attempt, a finished tool, a turn or session end. A Notification
    /// (including Grok's `task_complete`) is not evidence. The event clears an
    /// outstanding prompt only when it is newer than that prompt, and is kept
    /// so a prompt dispatched after it is not armed.
    fn note_dialog_closing_event(&self, incoming: &IncomingHookEvent) {
        let Some(run_id) = incoming.run_id.as_deref() else {
            return;
        };
        if !dialog_closed_by(&incoming.event) || !self.watched_kind_cached(run_id) {
            return;
        }
        let close_seq = incoming.connection.ingress_seq;
        let mut watches = self.lock_approval_watches();
        // Released between the check above and this lock: nothing to record.
        let Some(watch) = watches.get_mut(run_id) else {
            return;
        };
        if let Some(close_seq) = close_seq {
            watch.latest_close_seq = Some(watch.latest_close_seq.map_or(close_seq, |seen| seen.max(close_seq)));
        }
        let closes_outstanding =
            watch
                .outstanding
                .as_ref()
                .is_some_and(|open| match (open.prompt_ingress_seq, close_seq) {
                    (Some(prompt_seq), Some(close_seq)) => close_seq > prompt_seq,
                    _ => true,
                });
        if closes_outstanding {
            watch.outstanding = None;
        }
    }

    /// Fail `run_id` if the approval prompt that produced `mark` is still
    /// outstanding.
    ///
    /// The run counts as failed only once its durable failure is recorded and
    /// its pane teardown is verified. Anything short of that leaves the watch
    /// in place and returns [`UnansweredPromptOutcome::Incomplete`]: the
    /// tracking and the pool claim stay with the run until a retry completes,
    /// because a refused teardown can leave the pane alive. `progress` carries
    /// what earlier attempts did: once the failure is recorded the teardown is
    /// owed even if a dialog-closing event has since cleared the prompt, and
    /// only the first incomplete attempt and the settling one emit a dispatch
    /// event.
    pub(super) async fn fail_if_approval_prompt_unanswered(
        &self,
        run_id: &str,
        mark: &OutstandingApprovalPrompt,
        waited: Duration,
        progress: &mut UnansweredPromptProgress,
    ) -> UnansweredPromptOutcome {
        if !progress.failure_recorded
            && (self.outstanding_approval(run_id).as_ref() != Some(mark) || !self.is_deny_by_default_kind(run_id))
        {
            return UnansweredPromptOutcome::NotApplicable;
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
        let write = if self.execution_is_terminal(run_id) {
            Ok(())
        } else {
            self.completion_handler.fail_blocking_dialog(run_id, &detail).await
        };
        if let Err(error) = &write {
            tracing::error!(run_id, %error, "could not record the unanswered-approval-prompt failure");
        }
        // With the failure unrecorded the pane is left running: tearing it
        // down first would leave a dead worker on a nonterminal run, which
        // nothing then reconciles. The retry repeats the write, then releases.
        let release = if write.is_ok() {
            Some(self.release_worker_pane(run_id).await)
        } else {
            None
        };
        let recorded = write.is_ok() && self.execution_is_terminal(run_id);
        progress.failure_recorded |= recorded;
        let settled = recorded
            && (release == Some(PaneReleaseOutcome::Reaped) || self.worker_registry.slot_for_run(run_id).is_none());
        let mut details = serde_json::json!({
            "unanswered_approval_prompt": true,
            "driver": driver,
            "waited_secs": waited.as_secs(),
            "pane_release": format!("{release:?}"),
            "settled": settled,
        });
        if let Err(error) = &write {
            details["failure_write_error"] = serde_json::Value::String(error.to_string());
        }
        // Report the first incomplete attempt and the settling one; the
        // retries in between only log, so a failure that stays incomplete
        // does not flood the dispatch-event stream.
        if settled || !progress.incomplete_reported {
            let outcome = if settled { Outcome::Ok } else { Outcome::Error };
            let mut event = DispatchEvent::new(Stage::StaleWorkerReconcile, outcome, run_id).with_details(details);
            if let Ok(execution) = self.work_db.get_execution(run_id) {
                event = event.with_work_item(execution.work_item_id);
            }
            self.dispatch_events.emit(event).await;
            progress.incomplete_reported |= !settled;
        }
        if settled {
            self.forget_approval_watch(run_id);
            UnansweredPromptOutcome::Failed
        } else {
            tracing::error!(
                run_id,
                failure_recorded = progress.failure_recorded,
                ?release,
                "unanswered-approval-prompt: the run is not yet recorded failed with its pane verified released; \
                 keeping its tracking and pool claim and retrying"
            );
            UnansweredPromptOutcome::Incomplete
        }
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
