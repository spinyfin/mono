//! Which executions' preserved references are finished with.
//!
//! A `preserved/<execution>` bookmark keeps an unpublished tip alive until a
//! later execution has restaged it. Two facts the engine already records say
//! it is finished with, and neither needs a proof about commits:
//!
//! - a later execution that restaged it (`execution_bookmarks.recovered_from`,
//!   directly or through a chain of restages) has published, which stamps
//!   `work_executions.pr_head_after`;
//! - the item's PR chain has been closed (merged, closed or archived) for
//!   [`CLOSED_CHAIN_RETENTION_SECS`], so the work was either carried by a
//!   later execution or abandoned, and an open attention item already told the
//!   operator which.
use super::*;
use boss_engine_recovery::execution_bookmark::PreservedScope;
use boss_engine_storage_values::attention_kinds::PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND;

/// How long a closed item's preserved tip is kept before it is forgotten.
pub const CLOSED_CHAIN_RETENTION_SECS: i64 = 14 * 24 * 60 * 60;

/// `status, completed_at, archived_at, updated_at` of a chain root.
type ChainRootRow = (String, Option<String>, Option<String>, Option<String>);

/// Longest restage lineage followed from a published execution back to the
/// executions whose work it carries.
const MAX_LINEAGE_DEPTH: i64 = 16;

/// Why an execution's preserved references are finished with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreservedCleanupReason {
    /// A later execution that restaged this work has published.
    Superseded { published_by: String },
    /// The item's PR chain has been closed for the retention period.
    ClosedChain { closed_at_epoch: i64 },
}

/// An execution whose preserved tip can be forgotten.
#[derive(Debug, Clone)]
pub struct PreservedCleanup {
    pub execution: WorkExecution,
    pub reason: PreservedCleanupReason,
    /// Which of the execution's references to forget. A superseded tip goes
    /// alone: inspection archives hold variants that were not restaged. A
    /// closed chain's archives go with it, unless an operator was asked to
    /// inspect work teardown set aside and has not resolved the request.
    pub scope: PreservedScope,
}

#[work_db_boundary::instrument]
impl WorkDb {
    /// Executions whose preserved tip a later, published execution carried,
    /// with the published execution that vouches for each. At most `limit`.
    pub fn list_superseded_preserved(&self, limit: usize) -> Result<Vec<PreservedCleanup>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "WITH RECURSIVE lineage(published, ancestor, depth) AS (
                 SELECT eb.execution_id, eb.recovered_from, 1
                 FROM execution_bookmarks eb
                 JOIN work_executions we ON we.id = eb.execution_id
                 WHERE eb.recovered_from IS NOT NULL
                   AND we.pr_head_after IS NOT NULL AND we.pr_head_after != ''
                 UNION
                 SELECT l.published, eb.recovered_from, l.depth + 1
                 FROM lineage l
                 JOIN execution_bookmarks eb ON eb.execution_id = l.ancestor
                 WHERE eb.recovered_from IS NOT NULL AND l.depth < ?2
             )
             SELECT p.id, MIN(l.published)
             FROM lineage l
             JOIN work_executions p ON p.id = l.ancestor
             JOIN execution_bookmarks pe ON pe.execution_id = p.id
             WHERE p.preserved_commit_id IS NOT NULL AND p.id != l.published
             GROUP BY p.id
             ORDER BY p.id
             LIMIT ?1",
        )?;
        let pairs = stmt
            .query_map(params![limit as i64, MAX_LINEAGE_DEPTH], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let mut cleanups = Vec::with_capacity(pairs.len());
        for (id, published_by) in pairs {
            let execution = query_execution(&conn, &id)?.context("superseded execution disappeared")?;
            cleanups.push(PreservedCleanup {
                execution,
                reason: PreservedCleanupReason::Superseded { published_by },
                scope: PreservedScope::Tip,
            });
        }
        Ok(cleanups)
    }

    /// Terminal executions still holding a preserved tip whose PR chain has
    /// been closed for at least `retention_secs`. At most `limit`, oldest
    /// finished first.
    pub fn list_closed_chain_preserved(&self, retention_secs: i64, limit: usize) -> Result<Vec<PreservedCleanup>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT we.id FROM work_executions we
             JOIN execution_bookmarks eb ON eb.execution_id = we.id
             WHERE we.preserved_commit_id IS NOT NULL
               AND we.status IN ('completed', 'failed', 'cancelled', 'orphaned', 'abandoned')
             ORDER BY CAST(we.finished_at AS INTEGER) ASC, we.id ASC",
        )?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let cutoff = boss_engine_utils::epoch_time::now_epoch_secs() - retention_secs;
        let mut cleanups = Vec::new();
        for id in ids {
            if cleanups.len() >= limit {
                break;
            }
            let execution = query_execution(&conn, &id)?.context("closed-chain execution disappeared")?;
            let root = super::chain_helpers::chain_root(&conn, &execution.work_item_id)?;
            let closed_at: Option<ChainRootRow> = conn
                .query_row(
                    "SELECT status, completed_at, archived_at, updated_at FROM tasks WHERE id = ?1",
                    [&root],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((status, completed_at, archived_at, updated_at)) = closed_at else {
                continue;
            };
            let closed_at = match status.as_str() {
                "done" => completed_at.or(updated_at),
                "archived" => archived_at.or(completed_at).or(updated_at),
                _ => continue,
            };
            let Some(closed_at_epoch) = closed_at.and_then(|at| at.parse::<i64>().ok()) else {
                continue;
            };
            if closed_at_epoch > cutoff {
                continue;
            }
            let keep_inspection_archives: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM work_attention_items
                                WHERE execution_id = ?1 AND kind = ?2 AND status = 'open')",
                params![id, PRESERVED_WORK_NEEDS_INSPECTION_ATTENTION_KIND],
                |row| row.get(0),
            )?;
            cleanups.push(PreservedCleanup {
                execution,
                reason: PreservedCleanupReason::ClosedChain { closed_at_epoch },
                scope: if keep_inspection_archives {
                    PreservedScope::Tip
                } else {
                    PreservedScope::TipAndArchives
                },
            });
        }
        Ok(cleanups)
    }

    /// Record that `execution_id`'s preserved references were forgotten: its
    /// tip no longer exists to restage, and the attention items that pointed
    /// at those references (`resolve_kinds`) are finished with.
    pub fn finish_preserved_cleanup(&self, execution_id: &str, resolve_kinds: &[&str]) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE work_executions SET preserved_change_id = NULL, preserved_commit_id = NULL,
                preserved_commit_count = NULL WHERE id = ?1",
            params![execution_id],
        )?;
        let now = now_string();
        for kind in resolve_kinds {
            tx.execute(
                "UPDATE work_attention_items SET status = 'resolved', resolved_at = ?1
                 WHERE execution_id = ?2 AND kind = ?3 AND status = 'open'",
                params![now, execution_id, kind],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}
