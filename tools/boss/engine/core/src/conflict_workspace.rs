//! Recovery probes for a failed conflict workspace never rebase or publish.
use crate::work::{ConflictResolution, PendingMergeCheck, WorkDb};
use boss_execution_contracts::{CubeClient, ExecutionPublisher};

pub(crate) async fn recover(
    db: &WorkDb,
    publisher: &dyn ExecutionPublisher,
    cube: &dyn CubeClient,
    candidate: &PendingMergeCheck,
    attempt: &ConflictResolution,
    probe: &crate::merge_poller::PrLifecycleProbe,
) -> bool {
    let result = prepare(db, cube, candidate).await;
    if let Err(error) = result {
        tracing::warn!(attempt_id = %attempt.id, error = %format!("{error:#}"), "conflict workspace remains unavailable; retaining failed attempt");
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

async fn prepare(db: &WorkDb, cube: &dyn CubeClient, candidate: &PendingMergeCheck) -> anyhow::Result<()> {
    if let Some(writers) = crate::conflict_ladder::live_writers_blocking_rebase(db, &candidate.work_item_id) {
        anyhow::bail!("workspace recovery deferred while a worker owns the branch: {writers}");
    }
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
    let lease = cube.lease_workspace(&repo.repo_id, &label, None, false, &[]).await?;
    crate::ladder_lease_registry::register(&lease);
    let prepared = cube.goto_workspace(&lease.workspace_path, pr as u64).await;
    let released = cube.release_workspace(&lease.lease_id).await;
    crate::ladder_lease_registry::unregister(&lease.lease_id);
    // Neither a successful lease nor changed GitHub SHAs prove the bookmark
    // safe: only Cube's normal goto may make that decision.
    prepared?;
    released?;
    Ok(())
}
