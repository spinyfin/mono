//! Durable spawn confirmation, scoped to the current run. NULL means a
//! legacy launch: absence of the new marker must never authorize its reap.
use super::run_rows::resolve_run_id_for_execution_hooks;
use super::*;

#[work_db_boundary::instrument]
impl WorkDb {
    /// Arm before launching the pane. Retrying a spawn cannot erase success.
    pub fn begin_prompt_confirmation(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let Some(run) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(());
        };
        conn.execute(
            "UPDATE work_runs SET prompt_confirmed = 0 WHERE id = ?1 AND prompt_confirmed IS NULL",
            params![run],
        )?;
        Ok(())
    }

    pub fn finish_prompt_confirmation(&self, execution_id: &str) -> Result<()> {
        let conn = self.connect()?;
        if let Some(run) = resolve_run_id_for_execution_hooks(&conn, execution_id)? {
            conn.execute("UPDATE work_runs SET prompt_confirmed = 1 WHERE id = ?1", params![run])?;
        }
        Ok(())
    }

    /// A persisted turn also completes confirmation if the engine died before
    /// its wait returned. SessionStart alone deliberately does not count.
    pub fn prompt_confirmation_pending(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        Ok(conn.query_row(
            "SELECT COALESCE(prompt_confirmed = 0, 0) AND COALESCE(semantic_turn_condition NOT IN ('in_turn', 'ended'), 1) FROM work_runs WHERE id = ?1",
            params![run], |row| row.get(0),
        )?)
    }

    /// Unlike the recovery predicate, legacy NULL is not proof of a turn.
    pub fn prompt_acceptance_recorded(&self, execution_id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let Some(run) = resolve_run_id_for_execution_hooks(&conn, execution_id)? else {
            return Ok(false);
        };
        Ok(conn.query_row(
            "SELECT COALESCE(prompt_confirmed = 1, 0) OR COALESCE(semantic_turn_condition IN ('in_turn', 'ended'), 0) FROM work_runs WHERE id = ?1",
            params![run], |row| row.get(0),
        )?)
    }
}
