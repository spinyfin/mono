//! Global dispatch outage bookkeeping for host selection: classify a refused
//! selection as global, fold it into the outage record, and end the record on
//! the first dispatch that claims a host. See `tools/boss/docs/dispatch-outage.md`.
//! Part of the `coordinator` module split; see [`super`] for the struct.
use super::execution::HostSelectionFailure;
use super::*;

impl ExecutionCoordinator {
    /// Per-host exclusions when a refused selection found every registered
    /// host excluded for an item-independent reason (see
    /// [`classify_host_exclusions`]); `None` for an ordinary per-item refusal.
    pub(super) fn global_host_exclusions(
        &self,
        registry: &[Host],
        quarantine_reason: Option<&str>,
    ) -> Option<Vec<DispatchOutageHostExclusion>> {
        let local_hold = quarantine_reason.map(|reason| {
            let hold = self.work_db.read_local_worker_quarantine().unwrap_or_else(|err| {
                tracing::warn!(error = %format!("{err:#}"), "dispatch outage: could not re-read the local quarantine record");
                Default::default()
            });
            LocalHold {
                reason: reason.to_owned(),
                held_executions: hold.executions.keys().cloned().collect(),
                scan_error: hold.scan_error,
            }
        });
        classify_host_exclusions(registry, local_hold.as_ref())
    }

    /// Fold a refused host selection into the global dispatch outage record
    /// when it was a global refusal. The per-item failure handling that
    /// follows is unchanged: each item keeps its own retry, and this only
    /// adds the aggregate that makes the outage visible.
    pub(super) async fn record_dispatch_refusal(&self, execution: &WorkExecution, err: &anyhow::Error) {
        let Some(exclusions) = err
            .downcast_ref::<HostSelectionFailure>()
            .and_then(|failure| failure.global_exclusions.clone())
        else {
            return;
        };
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let effect =
            self.dispatch_outage
                .record_refusal(exclusions.clone(), &execution.work_item_id, &execution.id, now);
        if effect != RefusalEffect::Started {
            return;
        }
        tracing::error!(
            execution_id = %execution.id,
            work_item_id = %execution.work_item_id,
            hosts = %exclusions
                .iter()
                .map(|exclusion| format!("{}: {}", exclusion.host_id, exclusion.reason))
                .collect::<Vec<_>>()
                .join("; "),
            "dispatch outage started: every host is excluded, so no new worker can start",
        );
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::DispatchOutageStarted, DispatchOutcome::Error, "engine")
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "started_epoch_s": now,
                        "triggering_execution_id": execution.id,
                        "host_exclusions": exclusions,
                    })),
            )
            .await;
    }

    /// A dispatch just claimed a host. If a global outage was active, that
    /// is the recovery: record how long it lasted. Recovery is measured by
    /// this launch, never by a flag clearing.
    pub(super) async fn record_dispatch_claim(&self, execution: &WorkExecution, host_id: &str) {
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let Some(resolved) = self.dispatch_outage.record_claim(now) else {
            return;
        };
        tracing::info!(
            execution_id = %execution.id,
            host_id,
            duration_secs = resolved.duration_secs,
            refused_attempts = resolved.outage.refused_attempts,
            refused_work_items = resolved.outage.refused_work_items,
            "dispatch outage ended: a new dispatch claimed a host",
        );
        self.dispatch_events
            .emit(
                DispatchEvent::new(Stage::DispatchOutageRecovered, DispatchOutcome::Ok, "engine")
                    .with_work_item(&execution.work_item_id)
                    .with_details(serde_json::json!({
                        "started_epoch_s": resolved.outage.started_epoch_s,
                        "recovered_epoch_s": resolved.recovered_epoch_s,
                        "duration_secs": resolved.duration_secs,
                        "refused_attempts": resolved.outage.refused_attempts,
                        "refused_work_items": resolved.outage.refused_work_items,
                        "host_exclusions": resolved.outage.host_exclusions,
                        "claimed_by_execution_id": execution.id,
                        "claimed_host_id": host_id,
                    })),
            )
            .await;
    }
}
