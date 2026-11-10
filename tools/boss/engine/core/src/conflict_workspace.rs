//! Recovery probes for a failed conflict workspace never rebase or publish.
use crate::work::{ConflictResolution, PendingMergeCheck, WorkDb};
use anyhow::Result;
use async_trait::async_trait;
use boss_execution_contracts::{
    CubeClient, CubeRepoHandle, CubeWorkspaceLease, ExecutionPublisher, HostAdapter, HostAdapterProvider,
};
use std::path::Path;

/// The Cube operations a recovery probe needs, over either this engine's own
/// Cube or a remote host's adapter.
#[async_trait]
trait Probe: Send + Sync {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle>;
    async fn lease(&self, repo_id: &str, label: &str) -> Result<CubeWorkspaceLease>;
    async fn goto(&self, path: &Path, pr: u64) -> Result<()>;
    async fn release(&self, lease_id: &str) -> Result<()>;
}

#[async_trait]
impl Probe for &dyn CubeClient {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        CubeClient::ensure_repo(*self, origin).await
    }
    async fn lease(&self, repo_id: &str, label: &str) -> Result<CubeWorkspaceLease> {
        self.lease_workspace(repo_id, label, None, false, &[]).await
    }
    async fn goto(&self, path: &Path, pr: u64) -> Result<()> {
        self.goto_workspace(path, pr).await
    }
    async fn release(&self, lease_id: &str) -> Result<()> {
        self.release_workspace(lease_id).await
    }
}

#[async_trait]
impl Probe for std::sync::Arc<dyn HostAdapter> {
    async fn ensure_repo(&self, origin: &str) -> Result<CubeRepoHandle> {
        HostAdapter::ensure_repo(self.as_ref(), origin).await
    }
    async fn lease(&self, repo_id: &str, label: &str) -> Result<CubeWorkspaceLease> {
        self.lease_workspace(repo_id, label, None, false, &[]).await
    }
    async fn goto(&self, path: &Path, pr: u64) -> Result<()> {
        self.goto_workspace(path, pr).await
    }
    async fn release(&self, lease_id: &str) -> Result<()> {
        self.release_workspace(lease_id).await
    }
}

/// Probe preparation on the host whose refusal failed the attempt: this
/// engine's Cube for a local or mechanical-rung failure, the remote host's
/// adapter otherwise. A host that cannot be reached is reported on the
/// surfaced stop and leaves the failed attempt in place.
pub(crate) async fn recover(
    db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube: &dyn CubeClient,
    hosts: Option<&dyn HostAdapterProvider>,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    probe: &crate::merge_poller::PrLifecycleProbe,
) -> bool {
    let prepared = match db.conflict_workspace_failure_host(&attempt.id) {
        Ok(None) => prepare(db, &cube, candidate, true).await,
        Ok(Some(host_id)) => match remote_probe(db, hosts, &host_id).await {
            Ok(adapter) => prepare(db, &adapter, candidate, false).await,
            Err(error) => Err(error.context(format!("could not probe host {host_id}"))),
        },
        Err(error) => Err(error),
    };
    if let Err(error) = prepared {
        tracing::warn!(attempt_id = %attempt.id, error = %format!("{error:#}"), "conflict workspace remains unavailable; retaining failed attempt");
        if let Err(note_error) = db.note_workspace_recovery(
            &attempt.id,
            &format!("The last recovery probe did not succeed: {error:#}"),
        ) {
            tracing::warn!(attempt_id = %attempt.id, ?note_error, "could not record recovery probe result");
        }
        return false;
    }
    match db.resume_conflict_workspace(
        &attempt.id,
        probe.head_ref_oid.as_deref(),
        probe.base_ref_oid.as_deref(),
    ) {
        Ok(Some(_)) => {
            publisher
                .publish_work_item_changed(
                    &candidate.product_id,
                    &candidate.work_item_id,
                    "conflict_workspace_recovered",
                )
                .await;
            true
        }
        Ok(None) => false,
        Err(error) => {
            tracing::error!(attempt_id = %attempt.id, ?error, "could not resume recovered conflict workspace");
            false
        }
    }
}

async fn remote_probe(
    db: &WorkDb,
    hosts: Option<&dyn HostAdapterProvider>,
    host_id: &str,
) -> Result<std::sync::Arc<dyn HostAdapter>> {
    let hosts = hosts.ok_or_else(|| anyhow::anyhow!("no remote host adapters are available to this recovery path"))?;
    let host = db
        .get_host(host_id)?
        .ok_or_else(|| anyhow::anyhow!("host {host_id} is not registered"))?;
    hosts.adapter_for(&host).await
}

async fn prepare(db: &WorkDb, cube: &impl Probe, candidate: &PendingMergeCheck, local: bool) -> Result<()> {
    // Held until the probe finishes so no worker starts on the chain meanwhile.
    let _chain_hold = crate::conflict_ladder::hold_chain_for_engine_rebase(db, &candidate.work_item_id)
        .map_err(|writers| anyhow::anyhow!("workspace recovery deferred while a worker owns the branch: {writers}"))?;
    let repo = db
        .resolve_repo_for_task(&candidate.work_item_id)?
        .ok_or_else(|| anyhow::anyhow!("conflict workspace has no repository"))?;
    let pr = crate::merge_poller::stored_pr_number(&candidate.pr_url)
        .ok_or_else(|| anyhow::anyhow!("conflict workspace has no PR number"))?;
    let repo = cube.ensure_repo(&repo).await?;
    let label = format!(
        "{}{} {}",
        crate::conflict_ladder::RUNG1_TASK_LABEL_PREFIX,
        crate::ladder_lease_reap::engine_install_id(),
        candidate.work_item_id
    );
    let lease = cube.lease(&repo.repo_id, &label).await?;
    // The shutdown registry releases through the local Cube only.
    if local {
        crate::ladder_lease_registry::register(&lease);
    }
    let prepared = cube.goto(&lease.workspace_path, pr as u64).await;
    let released = cube.release(&lease.lease_id).await;
    if local {
        crate::ladder_lease_registry::unregister(&lease.lease_id);
    }
    // Neither a successful lease nor changed GitHub SHAs prove the bookmark
    // safe: only Cube's normal goto may make that decision.
    prepared?;
    released?;
    Ok(())
}
