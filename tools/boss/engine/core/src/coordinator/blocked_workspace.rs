use super::*;

impl ExecutionCoordinator {
    /// Restore the predecessor's engine-created reference into the new lease.
    /// No read of its old workspace or lease history occurs.
    ///
    /// A missing predecessor bookmark yields `None`: recovery is a precaution
    /// layered on top of dispatch and must never be able to break it. Restore
    /// failures of a recorded bookmark still fail the dispatch.
    pub(super) async fn recover_execution_bookmark(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
    ) -> Result<Option<(String, bool)>> {
        if let Some(record) = self.work_db.execution_bookmark_optional(&execution.id)? {
            // A dispatch retry already owns this reference. Validate and reuse
            // it instead of recreating refs or overwriting its provenance.
            let has_work = adapter
                .restore_execution_bookmark(&record, &lease.workspace_path)
                .await?;
            return Ok(Some((execution.id.clone(), has_work)));
        }
        let Some(prior) = self.work_db.recovery_predecessor(execution)? else {
            return Ok(None);
        };
        let Some(record) = self.work_db.execution_bookmark_optional(&prior.id)? else {
            self.warn_missing_execution_bookmark(execution, Some(&prior.id)).await;
            return Ok(None);
        };
        anyhow::ensure!(
            record.host_id == adapter.host_id(),
            "execution {} recovery store is on host {}; dispatch selected {}",
            prior.id,
            record.host_id,
            adapter.host_id()
        );
        let has_work = adapter
            .restore_execution_bookmark(&record, &lease.workspace_path)
            .await?;
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Ok, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "source": "execution_bookmark",
                        "predecessor": prior.id,
                        "bookmark": record.head(),
                        "has_work": has_work,
                    })),
            )
            .await;
        Ok(Some((prior.id, has_work)))
    }

    /// Loud, non-fatal: a missing pointer is visible in dispatch events and
    /// logs, but does not fail dispatch or resume. The abandoned-bookmark
    /// sweep is the place that still reports it as attention.
    pub(super) async fn warn_missing_execution_bookmark(&self, execution: &WorkExecution, predecessor: Option<&str>) {
        tracing::warn!(
            execution_id = %execution.id,
            predecessor,
            "no engine-created recovery bookmark recorded; continuing without recovery"
        );
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::WorkspaceRecovery, DispatchOutcome::Skipped, &execution.id)
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "source": "execution_bookmark",
                        "predecessor": predecessor,
                        "reason": "missing_bookmark",
                    })),
            )
            .await;
    }

    pub(crate) async fn inspect_execution_bookmark(
        &self,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
    ) -> Result<String> {
        let host = self
            .work_db
            .get_host(&record.host_id)?
            .ok_or_else(|| anyhow!("recovery host {} is unavailable", record.host_id))?;
        self.host_adapter_provider
            .adapter_for(&host)
            .await?
            .execution_bookmark_diff(record)
            .await
    }
}
