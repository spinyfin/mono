use super::*;
use boss_engine_recovery::execution_bookmark::pointer_integrity_error;

impl ExecutionCoordinator {
    /// Restore the predecessor's engine-created reference into the new lease.
    /// No read of its old workspace or lease history occurs.
    ///
    /// A recorded predecessor must have a durable pointer. Missing or unreadable
    /// pointers fail dispatch rather than silently discarding preserved work;
    /// those failures are typed (`PointerIntegrityError`) so the caller can tell
    /// them apart from transient fetch/goto/SSH failures, which stay retryable.
    ///
    /// `pr_for_goto` is the bound PR number already resolved by
    /// `pr_number_for_workspace_goto` (including the chain-root fallback), since
    /// resume executions do not carry `pr_url`.
    pub(super) async fn recover_execution_bookmark(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        pr_for_goto: Option<u64>,
    ) -> Result<Option<(String, bool)>> {
        let prior = if self.work_db.execution_bookmark_optional(&execution.id)?.is_some() {
            execution.clone()
        } else if let Some(prior) = self.work_db.recovery_predecessor(execution)? {
            prior
        } else {
            return Ok(None);
        };
        let Some(record) = self.work_db.execution_bookmark_optional(&prior.id)? else {
            self.warn_missing_execution_bookmark(execution, Some(&prior.id)).await;
            return Ok(None);
        };
        if record.host_id != adapter.host_id() {
            return Err(pointer_integrity_error(format!(
                "execution {} recovery store is on host {}; dispatch selected {}",
                prior.id,
                record.host_id,
                adapter.host_id()
            )));
        }
        let has_work = if prior.id != execution.id
            && matches!(
                execution.kind,
                ExecutionKind::ChoreImplementation
                    | ExecutionKind::TaskImplementation
                    | ExecutionKind::RevisionImplementation
            ) {
            let has_work = !adapter.execution_bookmark_diff(&record).await?.trim().is_empty();
            if execution.kind == ExecutionKind::RevisionImplementation && pr_for_goto.is_none() && !has_work {
                return Ok(None);
            }
            let repos = adapter.list_repos().await?;
            let repo = repos
                .iter()
                .find(|repo| repo.origin == execution.repo_remote_url)
                .ok_or_else(|| anyhow!("recovery repository is absent from cube repo list"))?;
            let base_branch = if execution.kind == ExecutionKind::RevisionImplementation {
                let pr = pr_for_goto
                    .ok_or_else(|| pointer_integrity_error("revision recovery requires its bound PR URL"))?;
                adapter.recovery_pr_base(&repo.origin, pr).await?
            } else {
                repo.main_branch.clone()
            };
            let pr_bookmark = if execution.kind == ExecutionKind::RevisionImplementation {
                let pr = pr_for_goto
                    .ok_or_else(|| pointer_integrity_error("revision recovery requires its bound PR URL"))?;
                // Cube resolves the bound PR head, fetches it, and writes pr/<n>.
                // The preserved execution pointer survives this checkout.
                adapter.goto_workspace(&lease.workspace_path, pr).await?;
                Some(format!("pr/{pr}"))
            } else {
                None
            };
            let report = adapter
                .restore_rebased_execution_bookmark(
                    &record,
                    &lease.workspace_path,
                    pr_bookmark.as_deref(),
                    &base_branch,
                )
                .await?;
            self.work_db.record_execution_restore_report(&execution.id, &report)?;
            has_work
        } else {
            adapter
                .restore_execution_bookmark(&record, &lease.workspace_path, prior.id == execution.id)
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
