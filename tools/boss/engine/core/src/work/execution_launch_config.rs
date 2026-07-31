use super::*;

impl WorkDb {
    /// Persist the resolved worker launch configuration exactly once per
    /// execution. This is deliberately stamped after the runner successfully
    /// launches the worker, not inferred later from current routing/model
    /// policy, so execution history remains an honest record of what ran.
    pub fn record_execution_launch_config(
        &self,
        execution_id: &str,
        driver: &str,
        model: &str,
        effort_level: Option<EffortLevel>,
    ) -> Result<WorkExecution> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let existing = query_execution(&tx, execution_id).require("execution", execution_id)?;
        if existing.driver.is_some() || existing.model.is_some() {
            tx.commit()?;
            return Ok(existing);
        }

        tx.execute(
            "UPDATE work_executions
             SET driver = ?2,
                 model = ?3,
                 effort_level = ?4
             WHERE id = ?1",
            params![execution_id, driver, model, effort_level.map(|level| level.as_str())],
        )?;
        let updated = query_execution(&tx, execution_id)?
            .with_context(|| format!("missing execution after launch-config update: {execution_id}"))?;
        tx.commit()?;
        Ok(updated)
    }
}
