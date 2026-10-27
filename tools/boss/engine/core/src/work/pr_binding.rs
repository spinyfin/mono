//! PR-binding integrity: the DB side of "a work item is bound only to the
//! PR its own execution branch opened".
//!
//! Three concerns live here:
//!
//! - [`WorkDb::other_work_item_bound_to_pr`] — the foreign-PR guard every
//!   primary binding path consults before writing `tasks.pr_url`. A PR
//!   already bound to a different work item can never be this execution's
//!   deliverable, whatever URL the worker's output carried.
//! - [`WorkDb::list_pr_binding_repair_candidates`] — rows whose stored
//!   binding is structurally suspect (bound to their own origin PR, sharing a
//!   PR with another work item, or carrying a doc pointer on a branch that is
//!   not their execution's). [`crate::pr_binding_repair_sweep`] confirms each
//!   against GitHub and repairs it by mechanism.
//! - [`WorkDb::rebind_task_pr_from_execution_branch`] /
//!   [`WorkDb::mark_task_pr_binding_unresolved`] — the two repair writes:
//!   re-point the row and its execution at the PR on the execution's own
//!   branch and put it into review, or park it visibly when no such PR
//!   exists.

use super::*;

/// `blocked_reason` stamped by the repair sweep when a row bound to a PR it
/// did not open has no PR on its execution's own branch either. The paired
/// `blocked_detail` names the branch and the rejected PR so the kanban card
/// says why the row is parked instead of showing a merged PR.
pub const BLOCKED_REASON_PR_BINDING_UNRESOLVED: &str = "pr_binding_unresolved";

/// A task whose stored PR binding or doc pointer looks structurally wrong,
/// paired with the latest implementation execution whose branch is the
/// durable pointer the binding should have come from.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct PrBindingRepairCandidate {
    pub task_id: String,
    pub product_id: String,
    pub task_status: TaskStatus,
    /// The PR the row currently shows.
    pub bound_pr_url: String,
    pub origin_pr_number: Option<i64>,
    pub doc_branch: Option<String>,
    pub execution_id: String,
    pub repo_remote_url: String,
    pub branch_naming: BranchNaming,
    pub worker_branch_prefix: Option<String>,
    /// Why the row was selected; informational, for logs and attention text.
    pub reasons: Vec<PrBindingSuspicion>,
    /// A `pr_review` / `pr_review_guide` execution is live on the row. The
    /// sweep may then only repair the doc pointer (which touches neither
    /// `pr_url` nor status); rebinding or parking waits for the review to end.
    #[builder(default)]
    pub has_live_review: bool,
    /// Expected branches of EVERY primary implementation execution of the
    /// row, newest first (see [`WorkDb::primary_execution_branches`]). A PR
    /// whose head matches any of them is the row's own: a resumed or
    /// bounced-back execution pushes to its predecessor's branch.
    #[builder(default)]
    pub own_branches: Vec<String>,
}

/// Structural signals, each computable from the DB alone, that a row's PR
/// binding or doc pointer did not come from its own execution branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrBindingSuspicion {
    /// `pr_url` is the row's own origin PR (`origin_pr_number`): a follow-up
    /// bound to the PR whose review spawned it.
    BoundToOriginPr,
    /// Another non-revision work item carries the same `pr_url`.
    SharedWithOtherWorkItem,
    /// `doc_branch` is an engine-shaped branch that does not identify this
    /// row's execution — doc-link detection ran against the wrong PR.
    DocBranchForeign,
}

impl PrBindingSuspicion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BoundToOriginPr => "bound_to_origin_pr",
            Self::SharedWithOtherWorkItem => "shared_with_other_work_item",
            Self::DocBranchForeign => "doc_branch_foreign",
        }
    }
}

/// Execution kinds that never open the work item's own PR; every other kind
/// is a primary implementation execution.
const NON_PRIMARY_EXECUTION_KINDS_SQL: &str = "'pr_review', 'pr_review_guide', 'revision_implementation', \
     'ci_remediation', 'conflict_resolution', 'automation_triage', 'answer_agent'";

/// Connection-level form of [`WorkDb::other_work_item_bound_to_pr`], so a
/// caller already inside a transaction (`Transaction` derefs to
/// `Connection`) applies the same holder policy.
pub(crate) fn query_other_work_item_bound_to_pr(
    conn: &rusqlite::Connection,
    pr_url: &str,
    work_item_id: &str,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT id FROM tasks
         WHERE pr_url = ?1
           AND id != ?2
           AND deleted_at IS NULL
           AND kind != 'revision'
         ORDER BY created_at ASC, id ASC
         LIMIT 1",
        params![pr_url, work_item_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

/// SQL predicate: `t.pr_url` is the PR number the row was spawned from.
const BOUND_TO_ORIGIN_SQL: &str = "(t.origin_pr_number IS NOT NULL AND t.pr_url LIKE '%/pull/' || t.origin_pr_number)";

/// SQL predicate: another live non-revision row holds `t.pr_url`.
const SHARED_PR_SQL: &str = "EXISTS (
                      SELECT 1 FROM tasks o
                      WHERE o.pr_url = t.pr_url
                        AND o.id != t.id
                        AND o.deleted_at IS NULL
                        AND o.kind != 'revision'
                    )";

#[work_db_boundary::instrument]
impl WorkDb {
    /// Expected branch names ([`crate::completion::expected_branch_name`]) of
    /// every primary implementation execution of `work_item_id`, newest first.
    /// A PR head matching any of them belongs to the work item: a resumed
    /// or bounced-back execution is told to push to its predecessor's branch,
    /// so ownership is decided by work item, not by the latest execution.
    pub fn primary_execution_branches(&self, work_item_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT id, branch_naming, worker_branch_prefix FROM work_executions
             WHERE work_item_id = ?1 AND kind NOT IN ({NON_PRIMARY_EXECUTION_KINDS_SQL})
             ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(params![work_item_id], |row| {
            let branch_naming: BranchNaming = deserialize_json_or_default(row.get::<_, Option<String>>(1)?.as_deref());
            let prefix = row.get::<_, Option<String>>(2)?.filter(|s| !s.is_empty());
            Ok(crate::completion::expected_branch_name(
                &row.get::<_, String>(0)?,
                &branch_naming,
                prefix.as_deref(),
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// The id of a different non-revision work item (in any status, including
    /// `done` / `archived`; only soft-deleted rows are ignored) already bound
    /// to `pr_url`, if any. Revision rows are excluded: a revision's `pr_url`
    /// is only ever its chain root's PR (stamped for the reviewer's benefit),
    /// so the root itself — a non-revision row — is what a conflict would
    /// surface as.
    pub fn other_work_item_bound_to_pr(&self, pr_url: &str, work_item_id: &str) -> Result<Option<String>> {
        query_other_work_item_bound_to_pr(&*self.connect()?, pr_url, work_item_id)
    }

    /// Rows whose PR binding or doc pointer is structurally suspect, each
    /// paired with its latest non-review implementation execution. Pure SQL
    /// plus branch-shape comparison — no network; the sweep confirms against
    /// GitHub before touching anything.
    ///
    /// Excludes rows with a live implementation-style execution (the running
    /// worker owns the binding) and rows a human already closed (`done` /
    /// `archived`). Engine-closed rows stay in: the merge poller closes a
    /// follow-up mis-bound to its merged origin PR as soon as it is
    /// `in_review`, which is exactly the row that needs repair. Only the
    /// binding signals apply to closed rows, never the doc pointer. A live
    /// review does not exclude a row — it is flagged via
    /// [`PrBindingRepairCandidate::has_live_review`] so only the doc-pointer
    /// repair runs while it is live.
    pub fn list_pr_binding_repair_candidates(&self) -> Result<Vec<PrBindingRepairCandidate>> {
        // Scoped so the connection is released before the per-row branch
        // lookups below take their own.
        let selected = {
            let conn = self.connect()?;
            let mut stmt = conn.prepare(&format!(
                "SELECT t.id, t.product_id, t.status, t.pr_url, t.origin_pr_number, t.doc_branch,
                    we.id, we.repo_remote_url, we.branch_naming, we.worker_branch_prefix,
                    {BOUND_TO_ORIGIN_SQL} AS bound_to_origin,
                    {SHARED_PR_SQL} AS shared,
                    EXISTS (
                      SELECT 1 FROM work_executions rv
                      WHERE rv.work_item_id = t.id
                        AND rv.kind IN ('pr_review', 'pr_review_guide')
                        AND rv.status IN (
                          'queued', 'ready', 'waiting_dependency', 'claimed', 'running',
                          'waiting_human', 'waiting_review', 'waiting_merge'
                        )
                    ) AS live_review
             FROM tasks t
             JOIN work_executions we ON we.id = (
               SELECT latest.id
               FROM work_executions latest
               WHERE latest.work_item_id = t.id
                 AND latest.kind NOT IN ({NON_PRIMARY_EXECUTION_KINDS_SQL})
               ORDER BY latest.created_at DESC, latest.id DESC
               LIMIT 1
             )
             WHERE t.deleted_at IS NULL
               AND t.kind IN ({CHORE_LIKE_KINDS_SQL})
               AND (
                 t.status NOT IN ('done', 'archived')
                 OR (
                   COALESCE(t.last_status_actor, '') = 'engine'
                   AND ({BOUND_TO_ORIGIN_SQL} OR {SHARED_PR_SQL})
                 )
               )
               AND t.pr_url IS NOT NULL
               AND t.pr_url != ''
               AND NOT EXISTS (
                 SELECT 1 FROM work_executions live
                 WHERE live.work_item_id = t.id
                   AND live.kind NOT IN ('pr_review', 'pr_review_guide')
                   AND live.status IN (
                     'queued', 'ready', 'waiting_dependency', 'claimed', 'running',
                     'waiting_human', 'waiting_review', 'waiting_merge'
                   )
               )
             ORDER BY t.created_at ASC, t.id ASC",
            ))?;
            let rows = stmt.query_map([], |row| {
                let branch_naming: BranchNaming =
                    deserialize_json_or_default(row.get::<_, Option<String>>(8)?.as_deref());
                let status: String = row.get(2)?;
                Ok((
                    PrBindingRepairCandidate {
                        task_id: row.get(0)?,
                        product_id: row.get(1)?,
                        task_status: status.parse::<TaskStatus>().unwrap_or(TaskStatus::Todo),
                        bound_pr_url: row.get(3)?,
                        origin_pr_number: row.get(4)?,
                        doc_branch: row.get::<_, Option<String>>(5)?.filter(|s| !s.is_empty()),
                        execution_id: row.get(6)?,
                        repo_remote_url: row.get(7)?,
                        branch_naming,
                        worker_branch_prefix: row.get::<_, Option<String>>(9)?.filter(|s| !s.is_empty()),
                        reasons: Vec::new(),
                        has_live_review: row.get::<_, bool>(12)?,
                        own_branches: Vec::new(),
                    },
                    row.get::<_, bool>(10)?,
                    row.get::<_, bool>(11)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut candidates = Vec::new();
        for (mut candidate, bound_to_origin, shared) in selected {
            if bound_to_origin {
                candidate.reasons.push(PrBindingSuspicion::BoundToOriginPr);
            }
            if shared {
                candidate.reasons.push(PrBindingSuspicion::SharedWithOtherWorkItem);
            }
            let closed = matches!(candidate.task_status, TaskStatus::Done | TaskStatus::Archived);
            if !candidate.reasons.is_empty() || (!closed && candidate.doc_branch.is_some()) {
                candidate.own_branches = self.primary_execution_branches(&candidate.task_id)?;
            }
            if !closed && let Some(doc_branch) = candidate.doc_branch.as_deref() {
                // Only an engine-shaped branch (`<prefix>/exec_…`) that names
                // NO primary execution of this row is evidence of a wrong PR;
                // `main`, a human branch, or the opaque-hash shape are left
                // alone, and a predecessor execution's branch is the row's own.
                let doc_suffix = crate::completion::branch_work_item_suffix(doc_branch);
                if doc_suffix.starts_with("exec_")
                    && !crate::completion::head_identifies_any_branch(doc_branch, &candidate.own_branches)
                {
                    candidate.reasons.push(PrBindingSuspicion::DocBranchForeign);
                }
            }
            if !candidate.reasons.is_empty() {
                candidates.push(candidate);
            }
        }
        Ok(candidates)
    }

    /// Clear a task's whole doc pointer (repo, branch, path). The repair
    /// sweep calls this before re-running doc detection on the correct PR:
    /// the detector never overwrites an existing `doc_path`, so a pointer
    /// copied off the wrong PR would otherwise survive.
    pub fn clear_task_doc_pointer(&self, task_id: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks
             SET doc_repo_remote_url = NULL, doc_branch = NULL, doc_path = NULL, updated_at = ?2
             WHERE id = ?1",
            params![task_id, now_string()],
        )?;
        Ok(())
    }

    /// Re-point `task_id` (and `execution_id`) at `new_pr_url`, the PR whose
    /// head is the execution's own branch, and move the row into review so
    /// the reviewer runs against the right PR. Guarded on the row still
    /// showing `old_pr_url` so a concurrent human fix is never overwritten.
    /// Returns whether the row changed.
    pub fn rebind_task_pr_from_execution_branch(
        &self,
        task_id: &str,
        execution_id: &str,
        old_pr_url: &str,
        new_pr_url: &str,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
             SET pr_url             = ?3,
                 status             = CASE WHEN status IN ('todo', 'active', 'blocked', 'done', 'archived') THEN 'in_review' ELSE status END,
                 completed_at       = CASE WHEN status IN ('done', 'archived') THEN NULL ELSE completed_at END,
                 blocked_reason     = NULL,
                 blocked_detail     = NULL,
                 blocked_attempt_id = NULL,
                 archived_by        = NULL,
                 archived_at        = NULL,
                 archived_reason    = NULL,
                 last_status_actor  = 'engine',
                 status_reason      = 'pr_binding_repair:rebound_to_execution_branch_pr',
                 updated_at         = ?4
             WHERE id = ?1
               AND deleted_at IS NULL
               AND pr_url = ?2
               AND NOT (status IN ('done', 'archived') AND COALESCE(last_status_actor, '') != 'engine')",
            params![task_id, old_pr_url, new_pr_url, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE work_executions SET pr_url = ?2 WHERE id = ?1",
            params![execution_id, new_pr_url],
        )?;
        let mut pending = PendingEvents::new();
        cascade_dependents_after_prereq_status_change(&mut pending, &tx, task_id, "in_review", &now)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(true)
    }

    /// Park `task_id` visibly: no PR exists on its execution's own branch, so
    /// the wrong binding is cleared (never left pointing at a PR the row did
    /// not open) and the row is `blocked:pr_binding_unresolved` with `detail`
    /// as its reason. Guarded on the row still showing `old_pr_url`. Returns
    /// whether the row changed.
    pub fn mark_task_pr_binding_unresolved(
        &self,
        task_id: &str,
        execution_id: &str,
        old_pr_url: &str,
        detail: &str,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let rows = tx.execute(
            "UPDATE tasks
             SET pr_url             = NULL,
                 status             = 'blocked',
                 blocked_reason     = ?3,
                 blocked_detail     = ?4,
                 blocked_attempt_id = NULL,
                 autostart          = 0,
                 completed_at       = NULL,
                 archived_by        = NULL,
                 archived_at        = NULL,
                 archived_reason    = NULL,
                 last_status_actor  = 'engine',
                 status_reason      = 'pr_binding_repair:foreign_pr_unbound',
                 updated_at         = ?5
             WHERE id = ?1
               AND deleted_at IS NULL
               AND pr_url = ?2
               AND NOT (status IN ('done', 'archived') AND COALESCE(last_status_actor, '') != 'engine')",
            params![task_id, old_pr_url, BLOCKED_REASON_PR_BINDING_UNRESOLVED, detail, now],
        )?;
        if rows == 0 {
            tx.commit()?;
            return Ok(false);
        }
        tx.execute(
            "UPDATE work_executions SET pr_url = NULL WHERE id = ?1 AND pr_url = ?2",
            params![execution_id, old_pr_url],
        )?;
        let mut pending = PendingEvents::new();
        cascade_dependents_after_prereq_status_change(&mut pending, &tx, task_id, "blocked", &now)?;
        commit_and_publish(tx, pending, &self.event_bus)?;
        Ok(true)
    }
}
