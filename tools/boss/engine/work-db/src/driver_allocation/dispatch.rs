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
        let result = refresh_execution_driver_decision_conn(&tx, execution_id);
        // Persist a failed-decision audit even when allocation is refused, so
        // launch composition records why the row cannot run. Other errors
        // roll the transaction back.
        if result.is_ok()
            || result
                .as_ref()
                .err()
                .is_some_and(|err| err.downcast_ref::<DriverAllocationRefused>().is_some())
        {
            tx.commit()?;
        }
        result
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
    let decision = match decide_execution_driver(conn, &execution.work_item_id, execution.kind) {
        Ok(decision) => decision,
        Err(err) => {
            if let Some(refused) = err.downcast_ref::<DriverAllocationRefused>() {
                let failed = DriverDecision::allocation_failed(refused.split_at_decision);
                if previous.as_ref() != Some(&failed) {
                    record_execution_driver_decision(
                        conn,
                        execution_id,
                        &execution.work_item_id,
                        &failed,
                        &now_string(),
                    )?;
                    tracing::info!(
                        execution_id,
                        ?previous,
                        ?failed,
                        "recorded allocation refusal before launch"
                    );
                }
            }
            return Err(err);
        }
    };
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
