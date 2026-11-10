//! Bounded history for read-only host observations.
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use crate::WorkDb;

impl WorkDb {
    /// Reuse an unanswered probe, otherwise refresh it. Retain the newest
    /// attempt across restarts, but discard older answered probes even while
    /// the assignment is live: they are observations, not ownership evidence.
    /// Receipt and sequence-allocation fences still apply to deletion.
    ///
    /// This performs one indexed query, never one connection/query per
    /// historical attempt. Only adopt/status callers may use this policy.
    pub fn refresh_host_probe(&self, host_id: &str, base: &str) -> Result<String> {
        anyhow::ensure!(
            base.starts_with("adopt-") || base.starts_with("status-"),
            "not a read-only probe"
        );
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lower = format!("{base}-a");
        let upper = format!("{base}-b");
        let latest: Option<(String, bool)> = tx
            .query_row(
                "SELECT command_id, settled FROM (
                 SELECT command_id, result IS NOT NULL AS settled FROM host_commands
                 WHERE host_id = ?1 AND command_id = ?2
                 UNION ALL
                 SELECT command_id, result IS NOT NULL AS settled FROM host_commands
                 WHERE host_id = ?1 AND command_id >= ?3 AND command_id < ?4)
             ORDER BY CAST(substr(command_id, length(?2) + 3) AS INTEGER) DESC LIMIT 1",
                params![host_id, base, lower, upper],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, settled)) = latest else {
            return Ok(base.to_owned());
        };
        tx.execute(
            "DELETE FROM host_commands
             WHERE host_id = ?1 AND command_id IN (
                 SELECT command_id FROM host_commands WHERE host_id = ?1 AND command_id = ?2
                 UNION ALL
                 SELECT command_id FROM host_commands WHERE host_id = ?1 AND command_id >= ?3 AND command_id < ?4)
               AND command_id != ?5 AND result IS NOT NULL
               AND (sequence IS NULL OR (
                   sequence <= (SELECT received FROM host_command_journals WHERE host_id = ?1)
                   AND sequence < (SELECT MAX(sequence) FROM host_commands WHERE host_id = ?1)))",
            params![host_id, base, lower, upper, id],
        )?;
        tx.commit()?;
        if !settled {
            return Ok(id);
        }
        let attempt = if id == base {
            0
        } else {
            id.strip_prefix(&lower).context("invalid probe id")?.parse::<u64>()?
        };
        Ok(format!(
            "{base}-a{}",
            attempt.checked_add(1).context("probe attempt overflow")?
        ))
    }
}
