//! Read helpers for the progression diagnosis packet (design 2.3).
//!
//! Kept out of `pr_flow.rs` / `progression_candidates.rs` so those files stay
//! under the line cap. Every method is a Deferred-style read: `connect()` and
//! query, no implicit transaction.

use super::*;
use crate::chain_helpers::collect_chain_revision_ids;
use rusqlite::params;

/// One `task_status_audit` row, newest first in listings.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct StatusAuditRow {
    pub property: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub actor: String,
    pub basis: Option<String>,
    pub changed_at: String,
}

/// One execution on the item or its revision chain, with the columns the
/// protocol `WorkExecution` mapper does not carry (`created_via`,
/// `run_done_outcome`) plus the latest run's `error_text`.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ExecutionDiagnosis {
    pub id: String,
    pub work_item_id: String,
    pub kind: String,
    pub status: String,
    pub created_via: Option<String>,
    pub last_error: Option<String>,
    pub run_done_outcome: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub latest_run_error: Option<String>,
}

/// Stored required-check snapshot on another of the product's PRs, used to
/// tell an unrelated or flaky failure from one this PR caused.
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ProductPrCiSnapshot {
    pub work_item_id: String,
    pub name: String,
    pub pr_url: String,
    pub status: String,
    pub ci_required_state: Option<String>,
    pub ci_required_detail: Option<String>,
}

/// How much of `progression.max_creates_per_item_window` this item has used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressionCreateBudget {
    pub used: i64,
    pub max: i64,
    pub window_secs: i64,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Ledger rows for the packet: the last `limit` `status` changes, plus the
    /// row that entered the current stuck state (the newest `blocked_reason` or
    /// `dispatch_failed_reason` write that set a value), deduplicated and
    /// returned newest first. Writes to other properties never crowd these out.
    pub fn list_recent_status_audit(&self, work_item_id: &str, limit: i64) -> Result<Vec<StatusAuditRow>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT property, old_value, new_value, actor, basis, changed_at
             FROM task_status_audit
             WHERE id IN (
                 SELECT id FROM (
                     SELECT id FROM task_status_audit
                     WHERE work_item_id = ?1 AND property = 'status'
                     ORDER BY id DESC LIMIT ?2)
                 UNION
                 SELECT id FROM (
                     SELECT id FROM task_status_audit
                     WHERE work_item_id = ?1
                       AND property IN ('blocked_reason', 'dispatch_failed_reason')
                       AND new_value IS NOT NULL
                     ORDER BY id DESC LIMIT 1))
             ORDER BY id DESC",
        )?;
        let rows = stmt.query_map(params![work_item_id, limit], |row| {
            Ok(StatusAuditRow {
                property: row.get(0)?,
                old_value: row.get(1)?,
                new_value: row.get(2)?,
                actor: row.get(3)?,
                basis: row.get(4)?,
                changed_at: row.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Latest `task_status_audit.basis` for `property` on this item.
    pub fn latest_status_audit_basis(&self, work_item_id: &str, property: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        crate::stuck_report::latest_basis(&conn, work_item_id, property)
    }

    /// Executions for `chain_root_id` and every live revision in its chain,
    /// oldest first, with provenance and the latest run error.
    pub fn list_execution_diagnoses_for_chain(&self, chain_root_id: &str) -> Result<Vec<ExecutionDiagnosis>> {
        let conn = self.connect()?;
        let mut ids = vec![chain_root_id.to_owned()];
        ids.extend(collect_chain_revision_ids(&conn, chain_root_id)?);
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT e.id, e.work_item_id, e.kind, e.status, e.created_via, e.last_error,
                    e.run_done_outcome, e.created_at, e.finished_at,
                    (SELECT r.error_text FROM work_runs r
                      WHERE r.execution_id = e.id
                      ORDER BY r.created_at DESC, r.id DESC LIMIT 1)
             FROM work_executions e
             WHERE e.work_item_id IN ({placeholders})
             ORDER BY e.created_at ASC, e.id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |row| {
            Ok(ExecutionDiagnosis {
                id: row.get(0)?,
                work_item_id: row.get(1)?,
                kind: row.get(2)?,
                status: row.get(3)?,
                created_via: row.get(4)?,
                last_error: row.get(5)?,
                run_done_outcome: row.get(6)?,
                created_at: row.get(7)?,
                finished_at: row.get(8)?,
                latest_run_error: row.get(9)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Required-check snapshots on the product's other PRs (open, or done
    /// inside the last seven days). Proxy for "this check's recent outcomes
    /// on main": a check that is red across PRs is likely unrelated or flaky.
    pub fn list_product_pr_ci_snapshots(
        &self,
        product_id: &str,
        exclude_work_item_id: &str,
    ) -> Result<Vec<ProductPrCiSnapshot>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, pr_url, status, ci_required_state, ci_required_detail
             FROM tasks
             WHERE product_id = ?1
               AND id != ?2
               AND deleted_at IS NULL
               AND pr_url IS NOT NULL
               AND (
                    status NOT IN ('done', 'archived')
                    OR (status = 'done' AND CAST(completed_at AS INTEGER)
                        > CAST(strftime('%s','now') AS INTEGER) - 7 * 86400)
               )
             ORDER BY updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![product_id, exclude_work_item_id], |row| {
            Ok(ProductPrCiSnapshot {
                work_item_id: row.get(0)?,
                name: row.get(1)?,
                pr_url: row.get(2)?,
                status: row.get(3)?,
                ci_required_state: row.get(4)?,
                ci_required_detail: row.get(5)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Revisions plus precursors minted with `created_via LIKE 'progression:%'`
    /// for this item inside the published rolling window. The count and limits
    /// come from the same SQL fragments the candidate clause uses.
    pub fn progression_create_budget(&self, work_item_id: &str) -> Result<ProgressionCreateBudget> {
        let conn = self.connect()?;
        let (max_sql, window_sql) = crate::progression_candidates::create_budget_limit_sqls();
        let used_sql = crate::progression_candidates::create_budget_used_sql("t", &window_sql);
        let sql = format!("SELECT ({used_sql}), ({max_sql}), ({window_sql}) FROM tasks t WHERE t.id = ?1");
        let (used, max, window_secs) = conn.query_row(&sql, params![work_item_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?))
        })?;
        Ok(ProgressionCreateBudget { used, max, window_secs })
    }

    /// Attention items on any of `work_item_ids`, directly or through one of
    /// their executions (a declared-blocked or nudge-breaker item attaches to
    /// the execution), each paired with the id of the work item that owns it.
    pub fn list_attention_items_for_work_items(
        &self,
        work_item_ids: &[String],
    ) -> Result<Vec<(String, boss_protocol::WorkAttentionItem)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at, converted_task_id
             FROM work_attention_items
             WHERE work_item_id = ?1
                OR execution_id IN (SELECT id FROM work_executions WHERE work_item_id = ?1)
             ORDER BY created_at ASC, id ASC",
        )?;
        let mut out = Vec::new();
        for id in work_item_ids {
            let rows = stmt.query_map([id], crate::mappers::map_attention_item)?;
            for item in collect_rows(rows)? {
                out.push((id.clone(), item));
            }
        }
        Ok(out)
    }

    /// The chain root plus every live revision in its chain.
    pub fn list_chain_work_item_ids(&self, chain_root_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut ids = vec![chain_root_id.to_owned()];
        ids.extend(collect_chain_revision_ids(&conn, chain_root_id)?);
        Ok(ids)
    }
}
