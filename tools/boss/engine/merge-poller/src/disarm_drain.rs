//! Turning off GitHub auto-merge for merge requests that conflict resolution stopped.
use super::host::SweepHost;
use super::*;

/// Turn off GitHub auto-merge for every `disarm_pending` intent, including
/// cards the ordinary candidate lists never visit (deletion sign-off, parked,
/// moved, rebound). Driven from the intent, not from task status.
///
/// A PR that has already merged or closed has no auto-merge left to turn off:
/// its pending disarm is retired instead of being retried on every sweep.
pub(crate) async fn drain_pending_merge_disarms(
    work_db: &WorkDb,
    probe: &dyn MergeProbe,
    publisher: &dyn ExecutionPublisher,
    host: &dyn SweepHost,
) {
    let pending = match work_db.list_pending_merge_disarms() {
        Ok(items) => items,
        Err(err) => {
            tracing::warn!(?err, "merge poller: failed to list pending merge disarms");
            return;
        }
    };
    if pending.is_empty() {
        return;
    }
    let mut urls: Vec<String> = pending.iter().map(|candidate| candidate.pr_url.clone()).collect();
    urls.sort();
    urls.dedup();
    let probed = probe.probe_batch(&urls).await;
    for candidate in &pending {
        let (terminal, observed) = match probed.get(&candidate.pr_url) {
            Some(Ok(observed)) => match observed.state {
                PrLifecycleState::Merged => (Some("merged"), Some(observed)),
                PrLifecycleState::ClosedUnmerged => (Some("closed"), Some(observed)),
                PrLifecycleState::Open(_) => (None, None),
            },
            _ => (None, None),
        };
        let Some(terminal) = terminal else {
            host.disarm_stopped_merge(work_db, publisher, candidate).await;
            continue;
        };
        match work_db.retire_pending_merge_disarm(&candidate.work_item_id, &candidate.pr_url, terminal) {
            Ok(true) => {
                tracing::info!(
                    work_item_id = %candidate.work_item_id,
                    pr_url = %candidate.pr_url,
                    terminal,
                    "merge poller: retired pending auto-merge disarm for a finished PR",
                );
                publisher
                    .publish_work_item_changed(&candidate.product_id, &candidate.work_item_id, "merge_disarm_retired")
                    .await;
                // The race the disarm exists for: GitHub merged the PR first.
                // Cards that stopped for a sign-off are in no poller list, so
                // nothing else would ever learn the merge happened.
                if terminal == "merged"
                    && let Some(observed) = observed
                {
                    super::sweep::mark_merged(work_db, publisher, Some(host), candidate, observed).await;
                }
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(
                work_item_id = %candidate.work_item_id,
                ?err,
                "merge poller: failed to retire pending auto-merge disarm",
            ),
        }
    }
}
