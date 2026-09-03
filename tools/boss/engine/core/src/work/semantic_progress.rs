//! Per-run semantic-progress checkpoint accessors.
//!
//! Columns live on `work_runs` for the same scoping reason as
//! `progress_ingress_checkpoint`: the stamp describes one spawned process,
//! so a later run must never inherit an earlier one's clock.

use super::run_rows::resolve_run_id_for_execution_hooks;
use super::*;

use crate::semantic_progress::{SemanticProgressCheckpoint, SemanticToolCondition, next_tool_condition};
use boss_protocol::WorkerEvent;

impl WorkDb {
    /// Record a driver-originated progress event against the run's
    /// agent-session row.
    ///
    /// Updates the last-progress timestamp on every call. The tri-state tool
    /// condition advances through [`next_tool_condition`]: session
    /// start/end leave an unknown row unknown. Redundant writes of the same
    /// `(timestamp, condition)` pair are skipped.
    ///
    /// Errors when the execution has no run row: a checkpoint nobody can
    /// read back is indistinguishable from no progress at all.
    pub fn record_semantic_progress(&self, execution_id: &str, event: &WorkerEvent) -> Result<()> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            bail!("no work_runs row for execution {execution_id}");
        };
        let previous = SemanticToolCondition::parse(
            conn.query_row(
                "SELECT semantic_tool_condition FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .as_deref(),
        );
        let condition = next_tool_condition(event, previous);
        let now = boss_engine_utils::iso8601::format_epoch_iso8601(boss_engine_utils::epoch_time::now_epoch_secs());
        conn.execute(
            "UPDATE work_runs
             SET semantic_progress_at = ?2, semantic_tool_condition = ?3
             WHERE id = ?1
               AND (semantic_progress_at IS NOT ?2 OR semantic_tool_condition IS NOT ?3)",
            params![run_id, now, condition.as_str()],
        )?;
        Ok(())
    }

    /// Read back the run's semantic-progress checkpoint.
    ///
    /// `Ok(None)` means the run never recorded one — a legitimate answer
    /// for a run dispatched by an engine that predates the columns, and a
    /// distinct one from a read failure, which surfaces as `Err`. A row
    /// whose timestamp is set but whose condition is NULL is returned as
    /// [`SemanticToolCondition::Unknown`].
    pub fn get_run_semantic_progress_checkpoint(
        &self,
        execution_id: &str,
    ) -> Result<Option<SemanticProgressCheckpoint>> {
        let conn = self.connect()?;
        let Some(run_id) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(None);
        };
        let row: Option<(Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT semantic_progress_at, semantic_tool_condition FROM work_runs WHERE id = ?1",
                params![run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(match row {
            Some((Some(progress_at), condition)) => Some(SemanticProgressCheckpoint {
                progress_at,
                tool_condition: SemanticToolCondition::parse(condition.as_deref()),
            }),
            Some((None, _)) | None => None,
        })
    }
}
