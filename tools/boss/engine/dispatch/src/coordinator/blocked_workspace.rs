use super::*;

/// What dispatch recovered into the new lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovered {
    pub predecessor: String,
    pub has_work: bool,
}

impl ExecutionCoordinator {
    /// Implementation recovery reads only teardown-preserved work. Reviews
    /// never inspect predecessor records or the shared recovery store.
    pub(super) async fn recover_execution_bookmark(
        &self,
        execution: &WorkExecution,
        lease: &CubeWorkspaceLease,
        adapter: &Arc<dyn HostAdapter>,
        repo_id: &str,
        pr_for_goto: Option<u64>,
        task_pr: Option<u64>,
    ) -> Result<Option<Recovered>> {
        if execution.kind.never_writes_pr_branch() {
            return Ok(None);
        }
        let source = self.work_db.recovery_source(execution)?;
        let Some(source) = source.filter(|source| source.has_preserved_tip) else {
            self.work_db.delete_execution_restore_report(&execution.id)?;
            if pr_for_goto.is_none()
                && let Some(pr) = task_pr
                && adapter.recovery_pr_is_open(repo_id, pr).await?
            {
                adapter.goto_workspace(&lease.workspace_path, pr).await?;
            }
            return Ok(None);
        };
        self.recover_preserved_work(
            execution,
            &source.execution,
            lease,
            adapter,
            repo_id,
            (pr_for_goto, task_pr),
        )
        .await
    }
    /// Whether `recorded` still holds unpublished work on the record's host.
    pub async fn preserved_work_unpublished(
        &self,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
        recorded: &boss_engine_recovery::execution_bookmark::PreservedTip,
    ) -> Result<bool> {
        let host = self
            .work_db
            .get_host(&record.host_id)?
            .ok_or_else(|| anyhow!("recovery host {} is unavailable", record.host_id))?;
        self.host_adapter_provider
            .adapter_for(&host)
            .await?
            .preserved_work_unpublished(record, recorded)
            .await
    }

    pub async fn inspect_execution_bookmark(
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

    /// Forget a finished execution's pointers once `vouchers` prove its
    /// work published, on whichever host holds the record.
    pub async fn prune_execution_bookmark(
        &self,
        record: &boss_engine_recovery::execution_bookmark::ExecutionBookmark,
        vouchers: &[boss_engine_recovery::execution_bookmark::Voucher],
        trunk: &str,
    ) -> Result<boss_engine_recovery::execution_bookmark::PruneOutcome> {
        let host = self
            .work_db
            .get_host(&record.host_id)?
            .ok_or_else(|| anyhow!("recovery host {} is unavailable", record.host_id))?;
        self.host_adapter_provider
            .adapter_for(&host)
            .await?
            .prune_execution_bookmark(record, vouchers, trunk)
            .await
    }

    /// The main branch cube records for `repo_id` on `host_id`, when the
    /// repo is registered there.
    pub async fn recovery_trunk(&self, host_id: &str, repo_id: &str) -> Result<Option<String>> {
        let host = self
            .work_db
            .get_host(host_id)?
            .ok_or_else(|| anyhow!("recovery host {host_id} is unavailable"))?;
        let repos = self
            .host_adapter_provider
            .adapter_for(&host)
            .await?
            .list_repos()
            .await?;
        Ok(repos
            .into_iter()
            .find(|repo| repo.repo_id == repo_id)
            .map(|repo| repo.main_branch))
    }
}
