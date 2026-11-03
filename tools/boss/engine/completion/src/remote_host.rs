//! Remote-host lookups shared by completion's teardown path and the engine's
//! host layer: resolving a durable host identity to its adapter, and removing
//! the staged input attempts a remote run left behind.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use boss_execution_contracts::{
    CubeChangeHandle, CubeClient, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus,
    ExecutionCubes, HostAdapter, HostAdapterProvider, ResolvedExecutionCube,
};

use crate::work::WorkDb;

/// Resolve a durable host identity once, validating the provider's routing.
pub async fn resolve_host_adapter(
    work_db: &WorkDb,
    provider: &dyn HostAdapterProvider,
    host_id: &str,
) -> Result<Arc<dyn HostAdapter>> {
    let host = work_db
        .get_host(host_id)?
        .with_context(|| format!("host '{host_id}' is no longer in the host registry"))?;
    let adapter = provider.adapter_for(&host).await?;
    anyhow::ensure!(
        adapter.host_id() == host_id,
        "host adapter for '{host_id}' reports host '{}'",
        adapter.host_id()
    );
    Ok(adapter)
}

/// Remove every staged input attempt recorded for `execution_id`, clearing
/// each marker once its attempt directory is gone. The directory is named by
/// the attempt id in the marker, not by the execution id.
pub async fn cleanup_execution_inputs(work_db: &WorkDb, adapter: &dyn HostAdapter, execution_id: &str) -> Result<()> {
    for item in work_db.pending_remote_input_cleanups_for_execution(execution_id)? {
        adapter.cleanup_initial_input(&item.workspace, &item.run_id).await?;
        work_db.clear_remote_input_cleanup(&item)?;
    }
    Ok(())
}

/// Production [`ExecutionCubes`]: routes by the host the execution's
/// current run was dispatched to.
///
/// `local` (and an execution with no run row yet) resolves to the engine's
/// own cube client unchanged, so the local-only path is byte-for-byte what
/// it was. Every other host resolves through the same
/// [`HostAdapterProvider`] the dispatch loop leases and releases with, so
/// the heartbeat rides the identical `ssh <host> cube …` `ControlMaster`
/// that issued the lease in the first place.
pub struct HostRoutedCubes {
    work_db: Arc<WorkDb>,
    provider: Arc<dyn HostAdapterProvider>,
    local: Arc<dyn CubeClient>,
}

impl HostRoutedCubes {
    pub fn new(work_db: Arc<WorkDb>, provider: Arc<dyn HostAdapterProvider>, local: Arc<dyn CubeClient>) -> Self {
        Self {
            work_db,
            provider,
            local,
        }
    }
}

#[async_trait]
impl ExecutionCubes for HostRoutedCubes {
    async fn cube_for_execution(&self, execution_id: &str) -> Result<ResolvedExecutionCube> {
        let host_id = self
            .work_db
            .latest_run_host_for_execution(execution_id)
            .with_context(|| format!("resolving the dispatch host for execution {execution_id}"))?;
        let host_id = match host_id.as_deref() {
            // No run row yet, or a local run: the engine's own cube issued
            // (or will issue) this lease.
            None | Some("local") => {
                return Ok(ResolvedExecutionCube {
                    cube: Arc::clone(&self.local),
                    host_id: "local".to_owned(),
                });
            }
            Some(host_id) => host_id.to_owned(),
        };
        let adapter = resolve_host_adapter(&self.work_db, self.provider.as_ref(), &host_id).await?;
        Ok(ResolvedExecutionCube {
            cube: Arc::new(HostAdapterCubeClient::new(adapter)),
            host_id,
        })
    }
}

// ── HostAdapterCubeClient ─────────────────────────────────────────────────────

/// Presents a [`HostAdapter`]'s cube half as a [`CubeClient`].
///
/// [`HostAdapter`] already declares every method [`CubeClient`] requires —
/// the trait was carved so that "which cube owns this workspace" is a
/// property of the host, not a global. But the engine's lease bookkeeping
/// predates host routing and is written against `&dyn CubeClient`
/// throughout (`cube_lease_heartbeat`,
/// `run_reconcile::confirm_execution_dead`). This newtype is the
/// adapter between the two, so a caller that has resolved the *owning*
/// host can hand its cube to that existing code unchanged instead of
/// silently falling back to the engine's own local cube.
///
/// Every method is a straight delegation — there is no `unimplemented!()`
/// arm and no behaviour of its own, which is what makes the substitution
/// safe: code that receives one of these cannot tell it apart from the
/// local `CommandCubeClient` except by which machine answers.
pub struct HostAdapterCubeClient {
    adapter: Arc<dyn HostAdapter>,
}

impl HostAdapterCubeClient {
    pub fn new(adapter: Arc<dyn HostAdapter>) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl CubeClient for HostAdapterCubeClient {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        self.adapter.ensure_repo(origin).await
    }

    async fn lease_workspace(
        &self,
        repo_id: &str,
        task: &str,
        prefer_workspace_id: Option<&str>,
        allow_dirty: bool,
        exclude_workspace_ids: &[&str],
    ) -> Result<CubeWorkspaceLease> {
        self.adapter
            .lease_workspace(repo_id, task, prefer_workspace_id, allow_dirty, exclude_workspace_ids)
            .await
    }

    async fn create_change(&self, workspace_path: &Path, title: &str) -> Result<CubeChangeHandle> {
        self.adapter.create_change(workspace_path, title).await
    }

    async fn goto_workspace(&self, workspace_path: &Path, pr: u64) -> Result<()> {
        self.adapter.goto_workspace(workspace_path, pr).await
    }

    async fn goto_workspace_revision(&self, workspace_path: &Path, sha: &str) -> Result<()> {
        self.adapter.goto_workspace_revision(workspace_path, sha).await
    }

    async fn release_workspace(&self, lease_id: &str) -> Result<()> {
        self.adapter.release_workspace(lease_id).await
    }

    async fn release_workspace_with_reset(&self, lease_id: &str) -> Result<()> {
        self.adapter.release_workspace_with_reset(lease_id).await
    }

    async fn workspace_status(&self, workspace_path: &Path) -> Result<CubeWorkspaceStatus> {
        self.adapter.workspace_status(workspace_path).await
    }

    async fn heartbeat_lease(&self, lease_id: &str, ttl_seconds: Option<u64>) -> Result<()> {
        self.adapter.heartbeat_lease(lease_id, ttl_seconds).await
    }

    async fn force_release_lease(&self, lease_id: &str, reason: Option<&str>) -> Result<()> {
        self.adapter.force_release_lease(lease_id, reason).await
    }

    async fn list_workspaces(&self) -> Result<Vec<CubeWorkspaceStatus>> {
        self.adapter.list_workspaces().await
    }

    async fn list_repos(&self) -> Result<Vec<CubeRepoSummary>> {
        self.adapter.list_repos().await
    }

    fn command_repr(&self, args: &[&str]) -> Option<(String, String)> {
        self.adapter.command_repr(args)
    }
}
