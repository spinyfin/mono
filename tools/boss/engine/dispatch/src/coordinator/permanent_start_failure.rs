//! The surfaces a permanent (non-retried) pre-start failure raises: attention
//! item, bounce to Backlog, dispatch event and app broadcasts. Shared by the
//! ordinary start-failure path and the driver-allocation refusal path so the
//! two cannot drift.
use super::*;

/// What failed and how it reads to an operator.
pub(super) struct StartFailureDetail<'a> {
    pub error: &'a anyhow::Error,
    pub error_text: &'a str,
    pub refusal_message: &'a str,
    pub positioning_refused: bool,
}

/// The attention item a permanent start failure files.
pub(super) struct StartFailureAttention<'a> {
    pub kind: &'a str,
    pub title: &'a str,
    /// Replaces the generic "could not start on worker" body.
    pub body: Option<String>,
}

/// Everything [`ExecutionCoordinator::surface_permanent_start_failure`] needs
/// about one permanently failed start.
pub(super) struct PermanentStartFailure<'a> {
    pub execution: &'a WorkExecution,
    pub run: Option<&'a WorkRun>,
    pub worker_id: &'a str,
    pub failure: StartFailureDetail<'a>,
    pub attention: StartFailureAttention<'a>,
}

impl ExecutionCoordinator {
    pub(super) async fn surface_permanent_start_failure(&self, surface: PermanentStartFailure<'_>) {
        let PermanentStartFailure {
            execution,
            run,
            worker_id,
            failure:
                StartFailureDetail {
                    error,
                    error_text,
                    refusal_message,
                    positioning_refused,
                },
            attention:
                StartFailureAttention {
                    kind: attention_kind,
                    title: attention_title,
                    body: attention_body,
                },
        } = surface;
        let execution = execution.clone();
        let run_id = run.map(|r| r.id.as_str()).unwrap_or("(none)");
        tracing::warn!(
            execution_id = %execution.id,
            run_id,
            worker_id,
            pre_start_failure_count = execution.pre_start_failure_count,
            error = %error,
            "recorded execution start failure"
        );

        // Maint task 6 — transient-retry wiring on `dispatch_not_before`:
        // an `automation_triage` execution that exhausts its pre-start
        // retries is the design's `failed_gave_up` terminal state.
        // Finalise the matching `automation_runs` row so the Automations
        // tab shows the occurrence was abandoned (the schedule already
        // advanced past it when the scheduler fired the triage). Until
        // this point the run sat at the pessimistic `failed_will_retry`.
        if execution.kind == ExecutionKind::AutomationTriage
            && let Err(err) = self.work_db.finalize_automation_triage_run(
                &execution.id,
                boss_protocol::AUTOMATION_OUTCOME_FAILED_GAVE_UP,
                None,
                Some(&format!(
                    "triage pre-start failed permanently after {} attempt(s): {error}",
                    execution.pre_start_failure_count
                )),
            )
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "failed to mark automation run failed_gave_up after permanent triage pre-start failure",
            );
        }

        // Surface every permanent pre-start failure as a
        // `WorkAttentionItem` so the failure is diagnosable in one
        // bossctl call instead of needing a tracing-log tail.
        let attention_body = attention_body.unwrap_or_else(|| {
            format!(
                "Execution `{execution_id}` could not start on worker `{worker_id}` \
                 after {attempts} attempt(s).\n\n\
                 **Error:** {shown_error}\n\n\
                 Inspect `dispatch-events/executions/{execution_id}/dispatch.jsonl` \
                 for the full stage timeline.",
                shown_error = if positioning_refused {
                    refusal_message
                } else {
                    error_text
                },
                execution_id = execution.id,
                attempts = execution.pre_start_failure_count,
            )
        });
        let attention_result = if positioning_refused && execution.work_item_id.starts_with("task_") {
            // The durable hold and deduplicated attention were committed together.
            Ok(())
        } else {
            self.work_db
                .create_attention_item(CreateAttentionItemInput {
                    execution_id: Some(execution.id.clone()),
                    work_item_id: None,
                    kind: attention_kind.to_owned(),
                    status: None,
                    title: attention_title.to_owned(),
                    body_markdown: attention_body,
                    resolved_at: None,
                })
                .map(|_| ())
        };
        if let Err(attention_err) = attention_result {
            tracing::error!(
                ?attention_err,
                execution_id = %execution.id,
                "failed to record attention item for execution start failure",
            );
        }

        self.notify_review_guide_pre_start_failure(&execution);

        // Stop the silent claim → fail → release → re-queue loop
        // (the "waiting for a slot" vs. "failing to start"
        // ambiguity): bounce the work item to Backlog with
        // `autostart` cleared and the failure reason/error stamped
        // directly on the row so the kanban card renders it
        // inline. Guarded on `status IN ('todo', 'active')`, so
        // this is a no-op for review-phase dispatch kinds
        // (`pr_review`, `ci_remediation`, `conflict_resolution`)
        // whose work item sits in `in_review`/`blocked` — bouncing
        // those would erase review context.
        //
        // A `progression` run is never the reason its item failed to
        // dispatch: it only reads the item, so its own start failure
        // must not move or stamp that item.
        let bounced_result = if !positioning_refused && execution.kind.holds_cube_lease() {
            self.work_db
                .bounce_dispatch_failed_to_backlog(&execution.work_item_id, attention_kind, error_text)
        } else {
            Ok(false)
        };
        let bounced = match bounced_result {
            Ok(true) => {
                tracing::info!(
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    reason = attention_kind,
                    "bounced work item to backlog after permanent pre-start dispatch failure",
                );
                true
            }
            Ok(false) => false,
            Err(bounce_err) => {
                tracing::error!(
                    ?bounce_err,
                    execution_id = %execution.id,
                    work_item_id = %execution.work_item_id,
                    "failed to bounce work item to backlog after permanent pre-start dispatch failure",
                );
                false
            }
        };
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::PreStartRetriesExhausted, DispatchOutcome::Error, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_worker(worker_id)
                    .with_error(error)
                    .with_details(serde_json::json!({
                        "reason": attention_kind,
                        "pre_start_failure_count": execution.pre_start_failure_count,
                        "bounced_to_backlog": bounced,
                    })),
            )
            .await;

        let publisher = self.publisher.clone();
        let execution_id = execution.id.clone();
        let work_item_id = execution.work_item_id.clone();
        let status_str = execution.status.as_str();
        // Route through `resolve_execution_work_item` rather than a
        // direct `get_work_item` call: `PrReviewGuide` (and
        // `AutomationTriage`/`AnswerAgent`) bind to a non-task id
        // that `get_work_item` can never resolve, so a direct call
        // here would log a spurious warning on every one of these
        // execution starts.
        let product_id = match self.resolve_execution_work_item(&execution) {
            Ok(item) => Some(item.product_id().to_string()),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    %work_item_id,
                    "failed to resolve product for runtime broadcast"
                );
                None
            }
        };
        boss_task_scope::spawn(async move {
            publisher
                .publish(&execution_id, &work_item_id, status_str, "execution_start_failed")
                .await;
            if let Some(product_id) = product_id {
                publisher
                    .publish_work_item_changed(&product_id, &work_item_id, "execution_start_failed")
                    .await;
            }
        });
    }
}
