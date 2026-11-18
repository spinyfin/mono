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

    /// Terminate an execution whose allocation was refused at dispatch, so it
    /// stops looping in `ready` on every drain. Fails the execution with the
    /// refusal as its recorded error, files an open attention item against
    /// it, and bounces the work item to Backlog with the reason stamped on
    /// the card (the same surfaces a permanent pre-start failure uses).
    /// Returns the failed execution.
    pub fn fail_execution_for_allocation_refusal(
        &self,
        execution_id: &str,
        refusal: &DriverAllocationRefused,
    ) -> Result<WorkExecution> {
        let error_text = format!("driver allocation refused: {refusal}");
        let (execution, _run, _outcome) =
            self.record_pre_start_failure(execution_id, "engine", None, &error_text, &[])?;
        self.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution.id.clone()),
            work_item_id: None,
            kind: DRIVER_ALLOCATION_REFUSED_ATTENTION_KIND.to_owned(),
            status: None,
            title: "No funded driver can run this work".to_owned(),
            body_markdown: format!(
                "Execution `{execution_id}` was not dispatched because no eligible driver holds a share of \
                 the traffic split.\n\n**Reason:** {refusal}\n\nRaise the share of an eligible driver \
                 (`bossctl dispatch traffic-split`), or change the item's model or driver pin, then start \
                 the item again."
            ),
            resolved_at: None,
        })?;
        if execution.kind.holds_cube_lease() {
            self.bounce_dispatch_failed_to_backlog(
                &execution.work_item_id,
                DRIVER_ALLOCATION_REFUSED_ATTENTION_KIND,
                &error_text,
            )?;
        }
        Ok(execution)
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
