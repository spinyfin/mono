//! What dispatch does when driver allocation is refused at claim time: fail
//! the one execution visibly instead of retrying it on every drain.
use super::permanent_start_failure::{PermanentStartFailure, StartFailureAttention, StartFailureDetail};
use super::*;

impl ExecutionCoordinator {
    /// When a dispatch claim failed because driver allocation was refused (no
    /// eligible driver holds a share), fail that one execution visibly: the
    /// execution ends `failed` with the refusal as its error, and the same
    /// permanent-failure surfaces a pre-start failure raises are applied (an
    /// attention item, a bounce to Backlog, a dispatch event and the app
    /// broadcasts). Without this the claim rolls back to `ready` and the same
    /// refusal repeats on every drain, invisibly.
    ///
    /// Returns whether `err` was an allocation refusal that was handled; any
    /// other claim error is left to the caller.
    pub(super) async fn fail_execution_if_allocation_refused(
        &self,
        execution: &WorkExecution,
        err: &anyhow::Error,
    ) -> bool {
        let Some(refusal) = err.downcast_ref::<crate::work::DriverAllocationRefused>() else {
            return false;
        };
        let error_text = format!("driver allocation refused: {refusal}");
        // No retries: a refusal is a configuration state, not a transient fault.
        let (failed, run, _outcome) =
            match self
                .work_db
                .record_pre_start_failure(&execution.id, "engine", None, &error_text, &[])
            {
                Ok(recorded) => recorded,
                Err(record_err) => {
                    tracing::error!(
                        execution_id = %execution.id,
                        ?record_err,
                        "failed to record driver allocation refusal",
                    );
                    return false;
                }
            };
        tracing::warn!(
            execution_id = %failed.id,
            work_item_id = %failed.work_item_id,
            reason = %refusal,
            "failed execution at dispatch: driver allocation refused",
        );
        let attention_body = format!(
            "Execution `{id}` was not dispatched because no eligible driver holds a share of the traffic \
             split.\n\n**Reason:** {refusal}\n\nRaise the share of an eligible driver \
             (`bossctl dispatch traffic-split`), or change the item's model or driver pin, then start \
             the item again.",
            id = failed.id,
        );
        self.surface_permanent_start_failure(PermanentStartFailure {
            execution: &failed,
            run: run.as_ref(),
            worker_id: "engine",
            failure: StartFailureDetail {
                error: err,
                error_text: &error_text,
                refusal_message: &error_text,
                positioning_refused: false,
            },
            attention: StartFailureAttention {
                suppress: false,
                kind: crate::work::DRIVER_ALLOCATION_REFUSED_ATTENTION_KIND,
                title: "No funded driver can run this work",
                body: Some(attention_body),
            },
        })
        .await;
        true
    }
}
