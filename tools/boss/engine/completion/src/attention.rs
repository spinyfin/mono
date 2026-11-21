//! Inherent methods on [`WorkerCompletionHandler`]; reaches the handler
//! struct, shared types, traits, and free helpers via `use super::*` (see
//! [`super`]).
//!
//! One helper: the execution-scoped attention-item filing shared by every
//! call site in this module tree that records something about a *run*
//! (worker escalation/blocker, deferred scope, proposal-channel error,
//! reviewer give-up, nudge-breaker park, mid-turn reap) rather than about
//! the work item. The work-item-scoped filings in `finalize_passes.rs` and
//! `pr_transition.rs` are deliberately NOT routed through here: they set
//! `work_item_id` instead of `execution_id` and publish no frontend event.

use super::*;

impl WorkerCompletionHandler {
    /// Atomically refresh or create an execution attention and notify the frontend.
    pub(super) async fn upsert_execution_attention(
        &self,
        execution: &crate::work::WorkExecution,
        kind: &str,
        title: &str,
        body_markdown: &str,
    ) -> Result<()> {
        let id = self
            .work_db
            .upsert_execution_attention(&execution.id, kind, title, body_markdown)?;
        if let Some(item) = self
            .work_db
            .list_attention_items(&execution.id)?
            .into_iter()
            .find(|item| item.id == id)
            && let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id)
        {
            self.publisher
                .publish_frontend_event_on_product(work_item.product_id(), FrontendEvent::AttentionItemCreated { item })
                .await;
        }
        Ok(())
    }

    /// File an execution-scoped attention item and publish the resulting
    /// `AttentionItemCreated` event on the work item's product.
    ///
    /// The item is scoped to the execution (not the work item): the store
    /// rejects setting both, and execution scoping is what makes
    /// `list_attention_items(&execution.id)` surface it — the lookup the
    /// nudge-suppression and dedup checks use.
    ///
    /// Returns the store's `Result` rather than logging failures itself:
    /// callers carry site-specific messages and severities (one is
    /// deliberately `error`, the rest `warn`), so the error arm stays with
    /// the caller. The publish is best-effort — a failed work-item lookup
    /// only costs the UI event, never the filing.
    pub(super) async fn file_execution_attention(
        &self,
        execution: &crate::work::WorkExecution,
        kind: &str,
        title: &str,
        body_markdown: String,
    ) -> Result<()> {
        let item = self.work_db.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: kind.to_owned(),
            status: None,
            title: title.to_owned(),
            body_markdown,
            resolved_at: None,
        })?;
        if let Ok(work_item) = self.work_db.get_work_item(&execution.work_item_id) {
            let product_id = work_item.product_id().to_string();
            self.publisher
                .publish_frontend_event_on_product(&product_id, FrontendEvent::AttentionItemCreated { item })
                .await;
        }
        Ok(())
    }

    /// Surface a missing review-outcome set as incomplete instead of a DB retry.
    pub(super) async fn stop_for_missing_review_outcomes(
        &self,
        execution_id: &str,
        err: &anyhow::Error,
    ) -> StopOutcome {
        let reason = err.to_string();
        match self.work_db.get_execution(execution_id) {
            Ok(execution) => {
                if let Err(attn_err) = self
                    .file_execution_attention(
                        &execution,
                        boss_engine_storage_values::attention_kinds::REVIEW_OUTCOMES_NOT_SUBMITTED_ATTENTION_KIND,
                        "Review outcomes not submitted",
                        reason.clone(),
                    )
                    .await
                {
                    tracing::warn!(
                        execution_id,
                        ?attn_err,
                        "failed to file missing review-outcomes attention"
                    );
                }
            }
            Err(load_err) => {
                tracing::warn!(
                    execution_id,
                    ?load_err,
                    "failed to load execution for missing review-outcomes attention"
                );
            }
        }
        StopOutcome::EscalationPending { reason }
    }
}
