//! Global dispatch outage bookkeeping for host selection: classify a refused
//! selection as global, fold it into the outage record, and end the record on
//! the first dispatch that claims a host. See `tools/boss/docs/dispatch-outage.md`.
//! Part of the `coordinator` module split; see [`super`] for the struct.
use super::execution::HostSelectionFailure;
use super::*;

/// "13 minutes ago"-style phrase for an outage instant, from the shared
/// relative-time helper; `"just now"` when the instant is not in the past.
/// The banner title and `bossctl` both render the outage start with it.
pub fn outage_elapsed_phrase(epoch_secs: i64, now_epoch_secs: i64) -> String {
    boss_engine_utils::iso8601::format_elapsed_ago(epoch_secs, now_epoch_secs).unwrap_or_else(|| "just now".to_owned())
}

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

    /// Exclusions for the global hold a failed startup worker scan puts on
    /// every work item, `None` when no scan error is recorded. Unlike a
    /// per-item quarantine this holds every host, so it is an outage whatever
    /// the registry looks like.
    pub(super) fn scan_error_hold_exclusions(&self) -> Option<Vec<DispatchOutageHostExclusion>> {
        let hold = self.work_db.read_local_worker_quarantine().ok()?;
        let scan_error = hold.scan_error.clone()?;
        let hosts = self.work_db.list_hosts().ok()?;
        let reason = self
            .work_db
            .local_dispatch_quarantine_reason()
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("every work item is quarantined: startup worker scan failed: {scan_error}"));
        let hold = LocalHold {
            reason,
            held_executions: hold.executions.keys().cloned().collect(),
            scan_error: Some(scan_error),
        };
        Some(scan_error_exclusions(&hosts, &hold))
    }

    /// Fold a refused host selection into the global dispatch outage record
    /// when it was a global refusal, before the refusal's own event is written
    /// so that event can name the outage it joined. The per-item failure
    /// handling that follows is unchanged: each item keeps its own retry, and
    /// this only adds the aggregate that makes the outage visible.
    pub(super) fn fold_dispatch_refusal(
        &self,
        execution: &WorkExecution,
        err: &anyhow::Error,
    ) -> Option<GlobalRefusal> {
        let exclusions = err
            .downcast_ref::<HostSelectionFailure>()
            .and_then(|failure| failure.global_exclusions.clone())?;
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let recorded =
            self.dispatch_outage
                .fold_refusal(exclusions.clone(), &execution.work_item_id, &execution.id, now);
        Some(GlobalRefusal {
            exclusions,
            recorded,
            work_item_id: execution.work_item_id.clone(),
            execution_id: execution.id.clone(),
        })
    }

    /// After the refusal's `host_selected` event is written: announce the
    /// outage if this refusal started it.
    pub(super) async fn announce_dispatch_outage(&self, refusal: &GlobalRefusal) {
        if refusal.recorded.effect != RefusalEffect::Started {
            return;
        }
        tracing::error!(
            execution_id = %refusal.execution_id,
            work_item_id = %refusal.work_item_id,
            hosts = %refusal
                .exclusions
                .iter()
                .map(|exclusion| format!("{}: {}", exclusion.host_id, exclusion.reason))
                .collect::<Vec<_>>()
                .join("; "),
            "dispatch outage started: every host is excluded, so no new worker can start",
        );
        self.dispatch_events.emit(outage_started_event(refusal)).await;
    }

    /// Global-health check for the startup-scan hold: a failed startup worker
    /// scan holds every work item, so a launch refused at admission or skipped
    /// by a reconcile/rescan pass never reaches host selection. Each such item
    /// is folded into the outage here, with the verbatim scan error, so the
    /// banner and `bossctl dispatch state` report it. A no-op without a scan
    /// error. Sync (events go out on a spawned task) so it can sit in the
    /// synchronous admission path.
    pub(super) fn note_scan_error_hold_refusals(&self, work_item_ids: &[String]) {
        if work_item_ids.is_empty() {
            return;
        }
        let Some(exclusions) = self.scan_error_hold_exclusions() else {
            return;
        };
        let now = boss_engine_utils::epoch_time::now_epoch_secs();
        let message = exclusions
            .first()
            .map(|exclusion| exclusion.reason.clone())
            .unwrap_or_default();
        let mut events = Vec::new();
        for work_item_id in work_item_ids {
            let refusal = GlobalRefusal {
                recorded: self
                    .dispatch_outage
                    .fold_refusal(exclusions.clone(), work_item_id, "", now),
                exclusions: exclusions.clone(),
                work_item_id: work_item_id.clone(),
                execution_id: String::new(),
            };
            // No execution exists yet: the refusal is filed under the engine.
            events.push(
                DispatchEvent::new(Stage::HostSelected, DispatchOutcome::Error, "engine")
                    .with_work_item(work_item_id)
                    .with_error(&anyhow!("{message}"))
                    .with_details(refusal.refusal_details()),
            );
            if refusal.recorded.effect == RefusalEffect::Started {
                tracing::error!(
                    work_item_id = %work_item_id,
                    "dispatch outage started: a failed startup worker scan holds every work item",
                );
                events.push(outage_started_event(&refusal));
            }
        }
        let sink = Arc::clone(&self.dispatch_events);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    for event in events {
                        sink.emit(event).await;
                    }
                });
            }
            Err(_) => tracing::warn!("dispatch outage recorded without a runtime to write its events"),
        }
    }

    /// The sweep form of [`Self::note_scan_error_hold_refusals`]: the active
    /// work the hold is keeping from dispatching. Reconcile, rescan and every
    /// launch path that asks the work db directly (a board drop to Doing,
    /// automation autostart, the engine sweeps) are refused by the hold
    /// without reaching the coordinator, so this runs at startup and on a
    /// timer ([`Self::spawn_scan_error_hold_watch`]). Only items the active
    /// outage has not counted are folded, so re-checking the same held items
    /// every pass does not inflate the attempt count.
    pub(super) fn note_scan_error_hold_blocking_active_work(&self) {
        if self.scan_error_hold_exclusions().is_none() {
            return;
        }
        match self.work_db.list_active_chores_without_live_run() {
            Ok(ids) => {
                let unseen = self.dispatch_outage.unrefused_work_items(&ids);
                self.note_scan_error_hold_refusals(&unseen);
            }
            Err(err) => tracing::warn!(error = %format!("{err:#}"), "dispatch outage: could not list held active work"),
        }
    }

    /// Re-check, every `interval`, for active work a failed startup worker
    /// scan is holding back. The hold is global and every refusal of it is
    /// silent on the paths named at [`Self::note_scan_error_hold_blocking_active_work`],
    /// so this is the one seam all of them share.
    pub fn spawn_scan_error_hold_watch(self: &Arc<Self>, interval: Duration) -> tokio::task::JoinHandle<()> {
        let coordinator = self.clone();
        boss_task_scope::spawn(async move {
            loop {
                coordinator.sweep_scan_error_hold();
                tokio::time::sleep(interval).await;
            }
        })
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

/// A refusal folded into the outage record, kept to build its events.
pub(super) struct GlobalRefusal {
    exclusions: Vec<DispatchOutageHostExclusion>,
    recorded: RecordedRefusal,
    work_item_id: String,
    execution_id: String,
}

impl GlobalRefusal {
    /// `details` of the refusal's `host_selected` error event. It names the
    /// outage it joined by start time, so a replay does not depend on the
    /// order concurrent dispatchers wrote their events in.
    pub(super) fn refusal_details(&self) -> serde_json::Value {
        serde_json::json!({
            "reason": "no_eligible_host",
            "global_outage": true,
            "outage_started_epoch_s": self.recorded.outage_started_epoch_s,
            "host_exclusions": self.exclusions,
        })
    }
}

fn outage_started_event(refusal: &GlobalRefusal) -> DispatchEvent {
    DispatchEvent::new(Stage::DispatchOutageStarted, DispatchOutcome::Error, "engine")
        .with_work_item(&refusal.work_item_id)
        .with_details(serde_json::json!({
            "started_epoch_s": refusal.recorded.outage_started_epoch_s,
            "triggering_execution_id": refusal.execution_id,
            "host_exclusions": refusal.exclusions,
        }))
}
