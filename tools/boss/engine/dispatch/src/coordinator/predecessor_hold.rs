//! The bounded wait on a recovery predecessor that still holds its cube
//! workspace lease. Part of the `coordinator` module split.

use super::*;

/// How long a kept-lease problem may persist before a recovery successor is
/// failed instead of held. The lease sweeps run every minute and
/// retry release, so this is thirty passes of margin over a transient fault.
pub(super) const PREDECESSOR_HOLD_BOUND_SECS: i64 = 30 * 60;

/// The result of the predecessor-lease check.
pub(super) enum PredecessorHold {
    /// Preservation may still complete: leave the row ready and wait.
    Waiting(String),
    /// The wait outlasted its bound: terminalize the row with this error.
    Failed(String),
}

impl ExecutionCoordinator {
    /// Why `execution` must not dispatch yet because the run it recovers from
    /// has not given up its cube workspace lease, or `None` when nothing is
    /// pending.
    ///
    /// A finished run's unpublished work is preserved as its lease is released,
    /// so while the lease is still recorded dispatch cannot tell "no work to
    /// restore" from "work not yet preserved". Dispatching anyway would start
    /// the successor from trunk and strand that work with nothing to restore
    /// it, so the row waits instead of guessing. A lookup that fails is held
    /// the same way: a row admitted without proof of what it recovers from
    /// would be the same guess. The lease holder's release (the lease-release
    /// paths preserve before releasing, and raise an attention item carrying
    /// the error if they cannot) is what ends the wait.
    ///
    /// The wait is bounded. A cleanup problem is often transient (an
    /// unreachable host, one failed probe) and the sweeps retry it, so a
    /// fresh attention item never fails the successor. A kept-lease item or a
    /// `workspace_lease_release_failed` item that has stayed open for
    /// [`PREDECESSOR_HOLD_BOUND_SECS`] does (the sweeps have then had many
    /// passes), or, for a remote predecessor that left no item at all, the
    /// same time since it ended. Either clock runs from the later of that
    /// start and the successor's first hold, so a successor created after the
    /// problem was already old still gets the full bound. A fresh open
    /// `workspace_lease_release_failed` item still blocks the missing-evidence
    /// fallback so a remote cube retry is not mistaken for silence. The check
    /// is read-only; callers record the failure with [`Self::fail_predecessor_hold`].
    pub(super) fn predecessor_lease_hold(&self, execution: &WorkExecution) -> Option<PredecessorHold> {
        match self.work_db.recovery_predecessor_holding_lease(execution) {
            Ok(None) => None,
            Ok(Some(predecessor)) => {
                let detail = format!(
                    "recovery predecessor {} still holds cube lease {}",
                    predecessor.id,
                    predecessor.cube_lease_id.as_deref().unwrap_or("(unrecorded)")
                );
                if !predecessor.status.is_terminal() {
                    return Some(PredecessorHold::Waiting(detail));
                }
                let now = boss_engine_utils::epoch_time::now_epoch_secs();
                // The bound runs from the later of the problem's own start and
                // the moment this successor first began waiting on it, so a
                // successor created after the problem was already old gets the
                // sweeps' full bound under the current cleanup code rather
                // than failing on its first check.
                let waiting_since = (execution.dispatch_wait_reason.as_deref()
                    == Some(DISPATCH_WAIT_REASON_AWAITING_PREDECESSOR_PRESERVATION))
                .then(|| execution.dispatch_wait_since.as_deref()?.parse::<i64>().ok())
                .flatten();
                let older_than_bound = |epoch: Option<i64>| {
                    epoch.zip(waiting_since).is_some_and(|(epoch, since)| {
                        now.saturating_sub(epoch.max(since)) >= PREDECESSOR_HOLD_BOUND_SECS
                    })
                };
                // Both kept-lease and cube-release-failed items are retried by
                // the sweeps. A fresh item never fails a successor. After the
                // bound, occupancy-gated cube failures are the same deadlock
                // as a kept lease: fail loudly with the item's text. While a
                // release-failed item is open and still inside the bound, the
                // missing-evidence fallback below must not fire either.
                let items = self.work_db.list_attention_items(&predecessor.id).ok();
                let open_of_kind = |kind: &str| {
                    items
                        .iter()
                        .flatten()
                        .find(|item| item.status == "open" && item.kind == kind)
                };
                let fail = |evidence: String| {
                    PredecessorHold::Failed(format!(
                        "Recovery cannot start: {detail} for more than {PREDECESSOR_HOLD_BOUND_SECS} seconds. {evidence}. Resolve the predecessor's workspace cleanup, then resume this execution. No workspace was reset."
                    ))
                };
                let release_failed =
                    open_of_kind(boss_engine_storage_values::attention_kinds::LEASE_RELEASE_FAILED_ATTENTION_KIND);
                if let Some(item) = release_failed {
                    return Some(if older_than_bound(item.created_at.parse::<i64>().ok()) {
                        fail(item.body_markdown.clone())
                    } else {
                        PredecessorHold::Waiting(detail)
                    });
                }
                let kept = open_of_kind(boss_engine_storage_values::attention_kinds::LEASE_UNRELEASED_ATTENTION_KIND);
                // A remote predecessor can leave no item at all (its worker pid
                // was never recorded, or the host was never probed). Nothing
                // would ever end that wait, so it is bounded by how long ago
                // the run ended.
                let remote = matches!(
                    self.work_db.latest_run_host_for_execution(&predecessor.id),
                    Ok(Some(host)) if host != "local"
                );
                let evidence = match kept {
                    Some(item) if older_than_bound(item.created_at.parse::<i64>().ok()) => item.body_markdown.clone(),
                    None if remote && older_than_bound(predecessor.finished_epoch()) => {
                        "No cleanup result was recorded for its remote worker".to_owned()
                    }
                    _ => return Some(PredecessorHold::Waiting(detail)),
                };
                Some(fail(evidence))
            }
            Err(err) => Some(PredecessorHold::Waiting(format!(
                "recovery predecessor could not be resolved: {err:#}"
            ))),
        }
    }

    /// Terminalize a row whose predecessor hold outlasted its bound. Never
    /// stamps the hold wait reason, and the terminalizing transaction clears
    /// any reason recorded while it waited: the row is no longer waiting.
    pub(super) fn fail_predecessor_hold(&self, execution: &WorkExecution, error: &str) {
        if let Err(err) = self
            .work_db
            .fail_execution_start(&execution.id, "recovery", None, error)
        {
            tracing::error!(execution_id = %execution.id, %err, "could not record recovery failure");
        }
        tracing::error!(execution_id = %execution.id, %error, "recovery predecessor release failed");
    }
}
