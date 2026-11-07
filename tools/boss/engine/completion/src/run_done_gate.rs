//! Silence-based declaration questions and visible parking.

use super::*;

impl WorkerCompletionHandler {
    /// Run the run-done backstop for an execution whose Stop reached
    /// [`SatisfiedDeliverableOutcome::AwaitingDeclaration`]: the bound PR
    /// looks satisfied but the worker has not declared its run finished.
    ///
    /// See [`crate::run_done_backstop`] for the full argument. In short:
    /// hold quietly while the worker shows life or the silence horizon has
    /// not elapsed, then ask (bounded by the existing circuit breaker),
    /// then — when the ask goes unanswered and the breaker parks the run —
    /// stamp `run_undeclared_at` and file a distinct attention so the
    /// ending is legible as "we never found out" rather than as a success.
    ///
    /// Never finalizes and never reaps. The only terminal-ish outcome it
    /// can reach is a park, which leaves the execution `waiting_human` for
    /// a human to redirect or re-dispatch.
    pub(super) async fn await_run_done_declaration(
        &self,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
        at_stop: bool,
    ) -> StopOutcome {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        if let Some(wait) = self.wait_registry.active(&execution.id, now) {
            // This is the machine-readable answer to our question. Start a
            // fresh silence window after it, with no unanswered-ask debt.
            self.run_done_silence_tracker.forget(&execution.id);
            self.nudge_breaker.forget(&execution.id);
            self.probe_queuer
                .clear_pending_turn_boundary_probes(&execution.id, "worker answered with a declared wait");
            // Keep the bounded wait observable even if no further Stop or
            // PR observation arrives. Its expiry re-enters this same gate.
            self.background_children_tracker.record_intent(
                &execution.id,
                BackgroundNudgeIntent {
                    probe_text: crate::run_done_backstop::PROBE_DECLARE_DONE.to_owned(),
                    fingerprint: format!("run_done_undeclared:{}", execution.id),
                    bound_pr_url: Some(bound_pr_url.to_owned()),
                    proceed_outcome: StopOutcome::AwaitingRunDoneDeclaration {
                        reason: "waiting for a declaration".to_owned(),
                    },
                    activity_watermark: self.background_activity_probe.activity_watermark(&execution.id),
                    hold: NudgeHold::WorkerWait,
                },
            );
            return StopOutcome::WorkerWaitPending {
                reason: wait.reason,
                expires_at_epoch: wait.expires_at_epoch,
            };
        }
        if self.work_db.list_attention_items(&execution.id).is_ok_and(|items| {
            items
                .iter()
                .any(|item| item.kind == RUN_UNDECLARED_ATTENTION_KIND && item.status != "resolved")
        }) {
            return StopOutcome::RunUndeclaredParked {
                reason: "run is awaiting human direction".to_owned(),
                waited_secs: 0,
            };
        }
        let activity = self
            .live_worker_states
            .as_ref()
            .and_then(|states| states.declaration_activity_for_run(&execution.id));
        let at_turn_boundary = activity.as_ref().map_or(at_stop, |activity| activity.at_turn_boundary);
        let last_activity_epoch = activity
            .as_ref()
            .and_then(|activity| activity.last_activity_at.as_deref())
            .and_then(boss_engine_utils::iso8601::parse_iso8601_to_epoch);
        let descendant_count = self
            .background_activity_probe
            .live_delegated_descendant_count(&execution.id)
            .unwrap_or_else(|reason| {
                tracing::debug!(
                    execution_id = %execution.id,
                    %reason,
                    "run_done backstop: background-child probe indeterminate — treating as no live \
                     descendants"
                );
                0
            });
        let decision = crate::run_done_backstop::decide(
            &self.run_done_silence_tracker,
            &execution.id,
            &execution.kind,
            crate::run_done_backstop::BackstopActivity {
                descendant_count,
                last_activity_epoch,
                at_turn_boundary,
                tool_in_flight: activity.as_ref().is_some_and(|activity| activity.tool_in_flight),
            },
            now,
        );
        match decision {
            crate::run_done_backstop::BackstopDecision::HoldingUntilStop => StopOutcome::AwaitingRunDoneDeclaration {
                reason: "waiting for the worker's next Stop before asking for a declaration".to_owned(),
            },
            crate::run_done_backstop::BackstopDecision::HoldingWorkerActive { descendant_count } => {
                tracing::info!(
                    execution_id = %execution.id,
                    bound_pr_url,
                    descendant_count,
                    "run_done backstop: no declaration yet, but the worker's process tree still has \
                     live descendants — it is working, not stalled. Holding quietly (no probe, no \
                     breaker, no finalize)",
                );
                StopOutcome::AwaitingRunDoneDeclaration {
                    reason: format!("worker has {descendant_count} live background child process(es)"),
                }
            }
            crate::run_done_backstop::BackstopDecision::HoldingWithinHorizon {
                waited_secs,
                horizon_secs,
            } => {
                tracing::debug!(
                    execution_id = %execution.id,
                    bound_pr_url,
                    waited_secs,
                    horizon_secs,
                    kind = %execution.kind,
                    "run_done backstop: no declaration yet and no sign of activity, but within the \
                     silence horizon — holding quietly",
                );
                StopOutcome::AwaitingRunDoneDeclaration {
                    reason: format!("silent for {waited_secs}s of a {horizon_secs}s declaration horizon"),
                }
            }
            crate::run_done_backstop::BackstopDecision::Ask {
                waited_secs,
                horizon_secs,
            } => {
                tracing::warn!(
                    execution_id = %execution.id,
                    bound_pr_url,
                    waited_secs,
                    horizon_secs,
                    kind = %execution.kind,
                    "run_done backstop: silence horizon elapsed with no `boss propose done` \
                     declaration and no sign of worker activity — asking the worker whether it is \
                     finished",
                );
                // The fingerprint is keyed on the *question*, not on any PR
                // state, so the breaker counts "asks that changed nothing"
                // — which is exactly the unproductive thing here. Keying it
                // on the PR URL (as the push-to-existing nudge does) would
                // collide with that nudge's own fingerprint and let one
                // path's asks consume the other's cap.
                let outcome = self
                    .nudge_or_park(
                        execution,
                        crate::run_done_backstop::PROBE_DECLARE_DONE,
                        &format!("run_done_undeclared:{}", execution.id),
                        Some(bound_pr_url),
                        StopOutcome::AwaitingRunDoneDeclaration {
                            reason: format!("asked for a declaration after {waited_secs}s of silence"),
                        },
                    )
                    .await;
                match outcome {
                    StopOutcome::NudgeBreakerParked { reason } => {
                        self.record_run_undeclared_park(execution, bound_pr_url, waited_secs, &reason)
                            .await;
                        StopOutcome::RunUndeclaredParked { reason, waited_secs }
                    }
                    other => other,
                }
            }
        }
    }

    /// An unanswered completion question is not evidence of worker failure.
    /// Keep the lease and pane attached and make the missing answer visible.
    pub(super) async fn park_undeclared_run(&self, execution: &crate::work::WorkExecution, count: u32) -> StopOutcome {
        let reason = format!("Run has not declared completion after {count} asks; awaiting human direction");
        if let Err(err) = self.work_db.set_execution_awaiting_human(&execution.id, true) {
            tracing::error!(execution_id = %execution.id, ?err, "run_done backstop: could not park execution");
            return StopOutcome::DbError;
        }
        self.publisher
            .publish(
                &execution.id,
                &execution.work_item_id,
                "waiting_human",
                "run_undeclared_parked",
            )
            .await;
        StopOutcome::NudgeBreakerParked { reason }
    }

    /// Stamp and surface a park that happened because a run never declared
    /// itself done.
    ///
    /// Two records, because the requirement is that this ending be
    /// distinguishable from a declared one *both* in stored state and to a
    /// human looking at the row:
    ///
    /// - `work_executions.run_undeclared_at` — the durable, queryable half.
    ///   A declared run has `run_done_declared_at`/`run_done_outcome` set
    ///   and this NULL; a backstopped run the reverse. Neither is inferred
    ///   from the absence of the other.
    /// - a [`RUN_UNDECLARED_ATTENTION_KIND`] attention — the human half,
    ///   specific to a missing completion declaration rather than a worker
    ///   failure.
    ///
    /// Both are best-effort: a failure here must not turn a park into an
    /// unhandled error, but it is logged loudly, because a park whose
    /// provenance was not recorded is exactly the silent ending this
    /// design exists to eliminate.
    async fn record_run_undeclared_park(
        &self,
        execution: &crate::work::WorkExecution,
        bound_pr_url: &str,
        waited_secs: i64,
        park_reason: &str,
    ) {
        RUN_DONE_BACKSTOP_PARKED.inc(&self.metrics);
        if let Err(err) = self.work_db.mark_execution_run_undeclared(&execution.id) {
            tracing::error!(
                execution_id = %execution.id,
                ?err,
                "run_done backstop: failed to stamp run_undeclared_at — this park will be \
                 indistinguishable in stored state from a declared completion",
            );
        }
        let already_filed = self
            .work_db
            .list_attention_items(&execution.id)
            .map(|items| {
                items
                    .iter()
                    .any(|i| i.kind == RUN_UNDECLARED_ATTENTION_KIND && i.status != "resolved")
            })
            .unwrap_or(false);
        if already_filed {
            return;
        }
        let body = format!(
            "This run is parked without the worker declaring it finished.\n\n\
             - execution: `{execution_id}`\n\
             - work item: `{work_item_id}`\n\
             - bound PR: {bound_pr_url}\n\
             - silent for: {waited_secs}s before the engine asked\n\n\
             The engine asked the worker to run `boss propose done` and got no declaration back \
             before the auto-nudge breaker tripped ({park_reason}).\n\n\
             **This is NOT a completion.** The run is parked, not `completed`, precisely because \
             nobody established that it finished: the bound PR's state cannot answer that question \
             for a run dispatched into an already-open PR, and the engine no longer pretends \
             otherwise. Decide what actually happened — read the transcript, check whether the PR \
             carries this run's work — and either re-dispatch or close it by hand.",
            execution_id = execution.id,
            work_item_id = execution.work_item_id,
        );
        if let Err(err) = self
            .file_execution_attention(
                execution,
                RUN_UNDECLARED_ATTENTION_KIND,
                "Run parked without declaring itself done",
                body,
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "run_done backstop: failed to file the undeclared-run attention item (non-fatal)",
            );
        }
    }
}
