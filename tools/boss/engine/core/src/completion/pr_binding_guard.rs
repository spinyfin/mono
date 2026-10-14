//! Foreign-PR guard: the last check before a primary execution binds a PR.
//!
//! Branch verification (`stop::verified_staged_pr_url`, the branch-keyed
//! detector) already guarantees the PR's head is this execution's branch.
//! This guard adds the one structural fact GitHub cannot tell us: whether
//! the engine has that PR bound to a *different* work item already. A PR
//! that belongs to another row can never be this execution's deliverable,
//! and binding it is exactly how a follow-up ended up showing another task's
//! merged PR (mono, 2026-10-13: a follow-up bound to mono#3074, another
//! task's merged PR) — the review-guide capture even logged "canonical PR …
//! is already associated with root task …" and the engine carried on.
//!
//! Inherent methods on [`WorkerCompletionHandler`]; see [`super`].

use super::*;

/// `work_attention_items.kind` filed when a primary execution's completion
/// is refused because the PR it resolved to is already bound to another
/// work item.
pub const FOREIGN_PR_REFUSED_ATTENTION_KIND: &str = "foreign_pr_binding_refused";

impl WorkerCompletionHandler {
    /// `Some(detail)` when `pr_url` is already bound to a live, non-revision
    /// work item other than `execution`'s own. Only meaningful for primary
    /// (non-revision) executions: a revision's deliverable IS its chain
    /// root's PR, which the root legitimately holds.
    ///
    /// A DB failure fails open (returns `None`) with a warning: this is a
    /// belt-and-suspenders check behind the branch verification, and a
    /// storage hiccup must not strand a correctly-verified completion.
    pub(super) fn foreign_pr_binding_conflict(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
    ) -> Option<String> {
        if execution.kind == ExecutionKind::RevisionImplementation {
            return None;
        }
        match self
            .work_db
            .other_work_item_bound_to_pr(pr_url, &execution.work_item_id)
        {
            Ok(Some(other_id)) => Some(format!(
                "the PR it resolved to, {pr_url}, is already bound to work item `{other_id}`. A PR that \
                 belongs to another work item is never this execution's deliverable, so nothing was \
                 bound."
            )),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(
                    execution_id = %execution.id,
                    pr_url,
                    ?err,
                    "foreign-PR guard: lookup failed; proceeding on branch verification alone",
                );
                None
            }
        }
    }

    /// Refuse a Stop-path / recheck-path completion whose PR belongs to
    /// another work item: fail the execution visibly (the task row records
    /// the diagnostic as its blocked reason) and file an attention item.
    pub(super) async fn refuse_foreign_pr_binding(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
        conflict: &str,
        source: &str,
    ) -> StopOutcome {
        let detail = format!(
            "Execution `{}` ({source}) could not be completed: {conflict} Check which work item \
             really owns {pr_url}; if this execution opened its own PR, re-bind it with \
             `boss task update --pr-url`.",
            execution.id
        );
        tracing::error!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            pr_url,
            source,
            "foreign-PR guard: refusing to bind a PR that belongs to another work item",
        );
        self.staged_pr_urls.forget(&execution.id);
        if !self.finalize_worker_failure(execution, &detail).await {
            return StopOutcome::DbError;
        }
        if let Err(err) = self
            .file_execution_attention(
                execution,
                FOREIGN_PR_REFUSED_ATTENTION_KIND,
                "Refused to bind a PR that belongs to another work item",
                detail.clone(),
            )
            .await
        {
            tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "foreign-PR guard: failed to file attention item",
            );
        }
        StopOutcome::ForeignPrRefused {
            pr_url: pr_url.to_owned(),
            detail,
        }
    }
}
