//! What dispatch does when driver allocation is refused at claim time: fail
//! the one execution visibly instead of retrying it on every drain.
use super::*;

impl ExecutionCoordinator {
    /// When a dispatch claim failed because driver allocation was refused (no
    /// eligible driver holds a share), fail that one execution visibly: the
    /// execution ends `failed` with the refusal as its error, an attention
    /// item is filed, the item is bounced to Backlog, and a dispatch event
    /// records it for `bossctl dispatch diagnose`. Without this the claim rolls
    /// back to `ready` and the same refusal repeats on every drain, invisibly.
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
        match self
            .work_db
            .fail_execution_for_allocation_refusal(&execution.id, refusal)
        {
            Ok(failed) => {
                tracing::warn!(
                    execution_id = %failed.id,
                    work_item_id = %failed.work_item_id,
                    reason = %refusal,
                    "failed execution at dispatch: driver allocation refused",
                );
                self.dispatch_events
                    .emit(
                        DispatchEvent::new(Stage::PreStartFailure, DispatchOutcome::Error, &execution.id)
                            .with_work_item(&execution.work_item_id)
                            .with_error(err)
                            .with_details(serde_json::json!({
                                "reason": crate::work::DRIVER_ALLOCATION_REFUSED_ATTENTION_KIND,
                                "will_retry": false,
                                "decision": "permanent",
                            })),
                    )
                    .await;
                true
            }
            Err(record_err) => {
                tracing::error!(
                    execution_id = %execution.id,
                    ?record_err,
                    "failed to record driver allocation refusal; execution stays ready",
                );
                false
            }
        }
    }
}
