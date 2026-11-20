//! Recovery probes for a failed conflict workspace never rebase or publish.
use crate::host_adapter::{HostAdapterCubeClient, resolve_host_adapter};
use crate::work::{ConflictResolution, PendingMergeCheck, WorkDb};
use anyhow::Result;
use boss_execution_contracts::{CubeClient, ExecutionPublisher, HostAdapterProvider};

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
    // A spent budget on an unchanged failure episode is not worth a probe:
    // keep the stop and say why it is not recovering.
    match db.workspace_resume_paused(
        &attempt.id,
        probe.head_ref_oid.as_deref(),
        probe.base_ref_oid.as_deref(),
    ) {
        Ok(true) => {
            publish_if_changed(
                db.note_workspace_recovery_paused(attempt),
                publisher,
                candidate,
                &attempt.id,
            )
            .await;
            return false;
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(attempt_id = %attempt.id, ?error, "could not read workspace resume budget"),
    }
    let prepared = match db.conflict_workspace_failure_host(&attempt.id) {
        Ok(None) => prepare(db, cube, candidate, true).await,
        Ok(Some(host_id)) => match async {
            let hosts =
                hosts.ok_or_else(|| anyhow::anyhow!("no remote host adapters are available to this recovery path"))?;
            resolve_host_adapter(db, hosts, &host_id)
                .await
                .map(HostAdapterCubeClient::new)
        }
        .await
        {
            Ok(adapter) => prepare(db, &adapter, candidate, false).await,
            Err(error) => Err(error.context(format!("could not probe host {host_id}"))),
        },
        Err(error) => Err(error),
    };
    if let Err(error) = prepared {
        tracing::warn!(attempt_id = %attempt.id, error = %format!("{error:#}"), "conflict workspace remains unavailable; retaining failed attempt");
        let probe_text = error
            .downcast_ref::<boss_host_execution::cube::CubeCliError>()
            .and_then(|error| error.refusal_message())
            .unwrap_or_else(|| format!("{error:#}"));
        publish_if_changed(
            db.note_workspace_recovery(
                &attempt.id,
                &format!("The last recovery probe did not succeed: {probe_text}"),
            ),
            publisher,
            candidate,
            &attempt.id,
        )
        .await;
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

/// A recovery note changes the card and the open attention; tell listeners
/// when it actually did.
async fn publish_if_changed(
    changed: Result<bool>,
    publisher: &dyn ExecutionPublisher,
    candidate: &PendingMergeCheck,
    attempt_id: &str,
) {
    match changed {
        Ok(true) => {
            publisher
                .publish_work_item_changed(
                    &candidate.product_id,
                    &candidate.work_item_id,
                    "conflict_workspace_noted",
                )
                .await
        }
        Ok(false) => {}
        Err(error) => tracing::warn!(attempt_id, ?error, "could not record recovery note"),
    }
}

async fn prepare(db: &WorkDb, cube: &dyn CubeClient, candidate: &PendingMergeCheck, local: bool) -> Result<()> {
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
    // The shutdown registry releases through the local Cube only.
    if local {
        crate::ladder_lease_registry::register(&lease);
    }
    let prepared = cube.goto_workspace(&lease.workspace_path, pr as u64).await;
    let released = cube.release_workspace(&lease.lease_id).await;
    if let Err(error) = &released {
        // Remote leases are not in the shutdown registry or the orphan reaper,
        // so the lease id in this line is the only handle to reclaim a leak.
        tracing::error!(
            lease_id = %lease.lease_id,
            workspace_id = %lease.workspace_id,
            local,
            error = %format!("{error:#}"),
            "failed to release a conflict recovery probe lease; it must be released by hand",
        );
    }
    if local {
        crate::ladder_lease_registry::unregister(&lease.lease_id);
    }
    // Neither a successful lease nor changed GitHub SHAs prove the bookmark
    // safe: only Cube's normal goto may make that decision.
    prepared?;
    released?;
    Ok(())
}
