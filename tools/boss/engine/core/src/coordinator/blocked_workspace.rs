use super::*;

impl ExecutionCoordinator {
    /// Restore the predecessor's engine-created reference into the new lease.
    /// No read of its old workspace or lease history occurs.
    ///
    /// A started predecessor must have a durable pointer. Missing or unreadable
    /// pointers fail dispatch rather than silently discarding preserved work.
    pub(super) async fn recover_execution_bookmark(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
    ) -> Result<Option<(String, bool)>> {
        let prior = if self.work_db.execution_bookmark_optional(&execution.id)?.is_some() {
            execution.clone()
        } else if let Some(prior) = self.work_db.recovery_predecessor(execution)? {
            prior
        } else {
            return Ok(None);
        };
        let record = self.work_db.execution_bookmark_optional(&prior.id)?.with_context(|| {
            format!(
                "expected engine-created recovery pointer for prior execution {}; no bookmark record exists",
                prior.id
            )
        })?;
        anyhow::ensure!(
            record.host_id == adapter.host_id(),
            "execution {} recovery store is on host {}; dispatch selected {}",
            prior.id,
            record.host_id,
            adapter.host_id()
        );
        let has_work = if matches!(
            execution.kind,
            ExecutionKind::ChoreImplementation
                | ExecutionKind::TaskImplementation
                | ExecutionKind::RevisionImplementation
        ) {
            let pr_bookmark = if execution.kind == ExecutionKind::RevisionImplementation {
                let pr = execution
                    .pr_url
                    .as_deref()
                    .and_then(boss_github::pr_url::pr_number_from_url)
                    .context("revision recovery requires its bound PR URL")?;
                // Cube resolves the bound PR head, fetches it, and writes pr/<n>.
                // The preserved execution pointer survives this checkout.
                adapter.goto_workspace(&lease.workspace_path, pr).await?;
                Some(format!("pr/{pr}"))
            } else {
                None
            };
            let has_work = !adapter.execution_bookmark_diff(&record).await?.trim().is_empty();
            let report = adapter
                .restore_rebased_execution_bookmark(&record, &lease.workspace_path, pr_bookmark.as_deref())
                .await?;
            self.work_db.record_execution_restore_report(&execution.id, &report)?;
            has_work
        } else {
            adapter
                .restore_execution_bookmark(&record, &lease.workspace_path)
                .await?
        };
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
