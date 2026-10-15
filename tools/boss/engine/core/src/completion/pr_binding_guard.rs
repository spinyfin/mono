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

/// A PR this execution verified as its own that the engine nonetheless has
/// bound to a different work item.
pub(super) struct ForeignPrConflict {
    /// The work item currently holding the PR. Because the PR's head is this
    /// execution's branch, that holder is the mis-bound row.
    pub holder_id: String,
    pub detail: String,
}

impl WorkerCompletionHandler {
    /// Expected branches of every primary implementation execution of
    /// `execution`'s work item, this execution's own first. A PR whose head
    /// identifies any of them ([`head_identifies_any_branch`]) belongs to the
    /// work item: a resumed or bounced-back execution is told to push to its
    /// predecessor's branch (`## RESUME EXISTING PR`), so keying ownership on
    /// this execution's id alone would fail every resume. A lookup failure
    /// degrades to the execution's own branch.
    pub(super) fn work_item_branches(&self, execution: &crate::work::WorkExecution) -> Vec<String> {
        let mut branches = vec![expected_branch_name(
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        )];
        match self.work_db.primary_execution_branches(&execution.work_item_id) {
            Ok(others) => {
                for b in others {
                    if !branches.contains(&b) {
                        branches.push(b);
                    }
                }
            }
            Err(err) => tracing::warn!(
                execution_id = %execution.id,
                ?err,
                "work-item branch lookup failed; verifying against this execution's branch only",
            ),
        }
        branches
    }

    /// `Some(conflict)` when `pr_url` is already bound to a non-revision work
    /// item (in any status, `done` / `archived` included) other than
    /// `execution`'s own. Only meaningful for primary
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
    ) -> Option<ForeignPrConflict> {
        if execution.kind == ExecutionKind::RevisionImplementation {
            return None;
        }
        match self
            .work_db
            .other_work_item_bound_to_pr(pr_url, &execution.work_item_id)
        {
            Ok(Some(other_id)) => Some(ForeignPrConflict {
                detail: format!(
                    "the PR it resolved to, {pr_url}, is already bound to work item `{other_id}`. A PR that \
                     belongs to another work item is never this execution's deliverable, so nothing was \
                     bound."
                ),
                holder_id: other_id,
            }),
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

    /// Refuse a completion (Stop, recheck or `run_done`) whose PR belongs to
    /// another work item: fail the execution visibly (the task row records
    /// the diagnostic as its blocked reason), file an attention item on it,
    /// and flag the holder. The PR's head is this execution's branch, so the
    /// holder is the mis-bound row; nothing else would select it for repair
    /// once this execution no longer shares the URL.
    pub(super) async fn refuse_foreign_pr_binding(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
        conflict: &ForeignPrConflict,
        source: &str,
    ) -> StopOutcome {
        let detail = format!(
            "Execution `{}` ({source}) could not be completed: {} Check which work item \
             really owns {pr_url}; if this execution opened its own PR, re-bind it with \
             `boss task update --pr-url`.",
            execution.id, conflict.detail
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
        self.flag_foreign_pr_holder(execution, pr_url, conflict);
        StopOutcome::ForeignPrRefused {
            pr_url: pr_url.to_owned(),
            detail,
        }
    }

    /// File a repair attention item on the work item holding a PR whose head
    /// is another execution's branch, with that evidence.
    fn flag_foreign_pr_holder(
        &self,
        execution: &crate::work::WorkExecution,
        pr_url: &str,
        conflict: &ForeignPrConflict,
    ) {
        let expected_branch = expected_branch_name(
            &execution.id,
            &execution.branch_naming,
            execution.worker_branch_prefix.as_deref(),
        );
        let body = format!(
            "This work item is bound to {pr_url}, but that PR's head branch is `{expected_branch}`, the branch of \
             execution `{}` (work item `{}`). The PR belongs to that work item, so this binding is wrong. Re-bind \
             this row with `boss task update --pr-url` or re-run it.",
            execution.id, execution.work_item_id,
        );
        if let Err(err) = self.work_db.upsert_external_tracker_attention(
            &conflict.holder_id,
            crate::pr_binding_repair_sweep::ATTENTION_KIND_PR_BINDING_REPAIR,
            "Bound to a PR this work item did not open",
            &body,
        ) {
            tracing::warn!(
                holder = %conflict.holder_id,
                ?err,
                "foreign-PR guard: failed to flag the mis-bound holder",
            );
        }
    }
}
