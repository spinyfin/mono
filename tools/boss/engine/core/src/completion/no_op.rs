//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.

use super::*;

impl WorkerCompletionHandler {
    /// Finalize a sanctioned no-op completion: the worker verified its work
    /// is already done (empty diff, no PR produced and none bound), so the
    /// task is closed cleanly as `done` WITHOUT a PR and the execution is
    /// finalised. No nudge is sent. Mirrors [`Self::finalize_pr_transition`]'s
    /// lease/pane release and event publishing, but never stamps a `pr_url`
    /// (there is none — fabricating one is the empty-PR the worker refused).
    ///
    /// Idempotent against an already-finalized execution: the DB write
    /// returns `None` for a non-live row, which maps to `AlreadyTerminal`.
    pub(super) async fn finalize_no_op_completion(&self, execution: &crate::work::WorkExecution) -> StopOutcome {
        let detail = "Worker verified the assigned work was already done (empty diff — no changes \
                      needed); closed as a no-op without a PR.";
        let completion = match self.work_db.record_worker_no_op_completion(&execution.id, detail) {
            Ok(Some(completion)) => completion,
            Ok(None) => return StopOutcome::AlreadyTerminal,
            Err(err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?err,
                    "no-op completion: failed to record",
                );
                return StopOutcome::DbError;
            }
        };
        // The worker reached a clean terminal — drop any staged URL and reset
        // the nudge counter so nothing lingers for this finalized execution.
        self.staged_pr_urls.forget(&execution.id);
        self.nudge_breaker.forget(&execution.id);
        self.build_wait_tracker.forget(&execution.id);
        if let Some(lease_id) = completion.released_lease_id.as_deref()
            && let Err(err) = self.cube_client.release_workspace(lease_id).await
        {
            tracing::error!(
                execution_id = %execution.id,
                lease_id,
                ?err,
                "no-op completion: cube release failed"
            );
        }
        self.pane_releaser.release_pane(&execution.id).await;
        let work_item_id = completion.execution.work_item_id.clone();
        self.publisher
            .publish(
                &completion.execution.id,
                &work_item_id,
                completion.execution.status.as_str(),
                "worker_no_op_completed",
            )
            .await;
        let product_id = completion.work_item.product_id().to_string();
        self.publisher
            .publish_work_item_changed(&product_id, &work_item_id, "worker_no_op_completed")
            .await;
        tracing::info!(
            execution_id = %execution.id,
            work_item_id = %work_item_id,
            kind = %execution.kind,
            "no-op completion: task closed as done without a PR (work already done)"
        );
        StopOutcome::NoChangesNeeded { work_item_id }
    }

    /// File a human-visible attention item recording that a reviewer worker
    /// exhausted its re-prompts without ever producing a readable
    /// `ReviewResult`, so its PR is advancing to Review **unreviewed**. Unlike
    /// [`Self::park_for_unproductive_nudges`], this does NOT change the
    /// execution's terminal handling — the caller still finalises the reviewer
    /// pass and advances the producing task — it only surfaces the give-up to
    /// the human. Best-effort: a filing failure is logged and swallowed.
    pub(super) async fn file_review_result_giveup_attention(
        &self,
        execution: &crate::work::WorkExecution,
        nudge_count: u32,
    ) {
        let body = format!(
            "The automated reviewer for this PR stopped {nudge_count} time(s) without writing a \
             valid ReviewResult — neither the structured-output artifact nor the transcript \
             fallback validated. The producing task is advancing to Review WITHOUT an automated \
             revision; review the PR by hand."
        );
        // Scope to the execution (not the work item): the store rejects
        // setting both, and execution-scoped mirrors the nudge-breaker
        // attention so `list_attention_items(&execution.id)` surfaces it.
        match self.work_db.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: REVIEW_RESULT_GIVEUP_ATTENTION_KIND.to_owned(),
            status: None,
            title: "Reviewer produced no valid ReviewResult".to_owned(),
            body_markdown: body,
            resolved_at: None,
        }) {
            Ok(item) => {
                if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
                    let product_id = work_item.product_id().to_string();
                    self.publisher
                        .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                        .await;
                }
            }
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    ?err,
                    "pr_review finalize: failed to file review-result give-up attention item",
                );
            }
        }
    }
}
