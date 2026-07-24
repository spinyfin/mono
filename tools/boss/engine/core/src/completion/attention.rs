//! Split out of `completion.rs`. Inherent methods on
//! [`WorkerCompletionHandler`]. Structural move only — no behavioural
//! change; see [`super`] for the handler struct, shared types, traits,
//! and free helpers this module reaches via `use super::*`.
//!
//! One helper: the execution-scoped attention-item filing shared by every
//! call site in this module tree that records something about a *run*
//! (worker escalation/blocker, deferred scope, proposal-channel error,
//! reviewer give-up, nudge-breaker park) rather than about the work item.
//! Work-item-scoped filings (`finalize_passes.rs`, `pr_transition.rs`) are
//! deliberately NOT routed through here: they set `work_item_id` instead of
//! `execution_id` and publish no frontend event.

use super::*;

impl WorkerCompletionHandler {
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
}
