//! Revalidate saved allocations at dispatch boundaries, never on hook reads.

use super::*;

#[work_db_boundary::instrument]
impl WorkDb {
    /// Prepare the durable routing decision immediately before composing a
    /// new launch. A share can be disabled while cube prepares the workspace,
    /// after the initial dispatch claim. Running workers keep their routing:
    /// ordinary decision reads never call this method.
    pub fn refresh_execution_driver_decision(&self, execution_id: &str) -> Result<Option<DriverDecision>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let decision = refresh_execution_driver_decision_conn(&tx, execution_id)?;
        tx.commit()?;
        Ok(decision)
    }
}

pub(crate) fn refresh_execution_driver_decision_conn(
    conn: &Connection,
    execution_id: &str,
) -> Result<Option<DriverDecision>> {
    let previous = get_execution_driver_decision_conn(conn, execution_id)?;
    let Some(execution) = query_execution(conn, execution_id)? else {
        return Ok(previous);
    };
    let decision = decide_execution_driver(conn, &execution.work_item_id, execution.kind)?;
    if previous.as_ref() == Some(&decision) {
        return Ok(previous);
    }
    record_execution_driver_decision(conn, execution_id, &execution.work_item_id, &decision, &now_string())?;
    tracing::info!(
        execution_id,
        ?previous,
        ?decision,
        "revalidated driver allocation before launch"
    );
    Ok(Some(decision))
}
