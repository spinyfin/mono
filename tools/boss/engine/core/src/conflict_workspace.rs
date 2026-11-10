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
    // The local Cube cannot vouch for a remote host's repository. A refusal
    // that came from another host is only retried once GitHub shows the head
    // or base moved; otherwise the failed attempt stays surfaced.
    if let Some(host) = failing_remote_host(db, attempt) {
        let moved = probe
            .head_ref_oid
            .as_deref()
            .is_some_and(|head| attempt.head_sha_before.as_deref() != Some(head))
            || probe
                .base_ref_oid
                .as_deref()
                .is_some_and(|base| attempt.base_sha_at_trigger.as_deref() != Some(base));
        if !moved {
            tracing::warn!(attempt_id = %attempt.id, %host, "conflict workspace refusal came from a remote host; retaining failed attempt until head or base moves");
            return false;
        }
    } else if let Err(error) = prepare(db, cube, candidate).await {
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

/// A host other than this engine's own where the failed revision was placed.
fn failing_remote_host(db: &WorkDb, attempt: &ConflictResolution) -> Option<String> {
    let revision = attempt.revision_task_id.as_deref()?;
    db.execution_host_ids_for_item(revision)
        .ok()?
        .into_values()
        .find(|host| host != boss_protocol::LOCAL_HOST_ID)
}

async fn prepare(db: &WorkDb, cube: &dyn CubeClient, candidate: &PendingMergeCheck) -> anyhow::Result<()> {
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
