//! Engine-owned reconciliation of app presentation against execution truth.

use super::*;
use boss_protocol::{AttachWorkerPaneInput, AttachWorkerPaneResult, HostedPaneEntry};

/// Result of one hosted-pane inventory pass. Failures are recorded per pane
/// so a single unconfirmed detach cannot skip attaching every other live worker.
pub(super) struct WorkerViewerReconcile {
    pub live: HashSet<String>,
    pub blocked_slots: HashSet<u8>,
    pub had_failures: bool,
}

impl ServerState {
    pub(super) async fn hosted_worker_viewers(&self) -> Result<Option<Vec<HostedPaneEntry>>, String> {
        match self
            .send_to_app(
                EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {}),
                Duration::from_secs(5),
            )
            .await
        {
            Ok(EngineToAppResponse::ListHostedPanes { result: Ok(result) }) => Ok(Some(result.panes)),
            // Registration reconciliation owns viewers when the app reconnects.
            Err(SendToAppError::NotRegistered) => Ok(None),
            other => Err(format!("cannot inventory app viewers: {other:?}")),
        }
    }

    fn viewer_is_stale(&self, run_id: &str) -> anyhow::Result<bool> {
        // Missing rows are stale; database failures are not evidence of death.
        Ok(self
            .work_db
            .find_execution(run_id)?
            .is_none_or(|run| run.status.is_terminal()))
    }

    async fn detach_viewer(&self, slot_id: u8) -> Result<(), String> {
        match self
            .send_to_app(
                EngineToAppRequest::DetachWorkerPane(crate::protocol::DetachWorkerPaneInput { slot_id }),
                PANE_RELEASE_ACK_TIMEOUT,
            )
            .await
        {
            Ok(EngineToAppResponse::DetachWorkerPane { result: Ok(_) })
            | Ok(EngineToAppResponse::DetachWorkerPane {
                result: Err(EngineToAppError::UnknownSlot),
            }) => Ok(()),
            other => Err(format!("viewer detach for slot {slot_id} unconfirmed: {other:?}")),
        }
    }

    /// Inventory and detach under the same lock as attaches: a stale inventory
    /// must never remove a viewer that a concurrent spawn just installed.
    pub(super) async fn reconcile_worker_viewers(&self) -> Result<WorkerViewerReconcile, String> {
        let _guard = self.attach_pane_lock.lock().await;
        let mut live = HashSet::new();
        let mut blocked_slots = HashSet::new();
        let mut had_failures = false;
        for pane in self.hosted_worker_viewers().await?.unwrap_or_default() {
            match self.viewer_is_stale(&pane.run_id) {
                Ok(true) => match self.detach_viewer(pane.slot_id).await {
                    Ok(()) => {
                        tracing::info!(run_id = %pane.run_id, slot_id = pane.slot_id, "detached stale app viewer");
                    }
                    Err(err) => {
                        had_failures = true;
                        blocked_slots.insert(pane.slot_id);
                        tracing::warn!(
                            run_id = %pane.run_id,
                            slot_id = pane.slot_id,
                            error = %err,
                            "stale viewer detach unconfirmed; skipping this slot and continuing"
                        );
                    }
                },
                Ok(false) => {
                    live.insert(pane.run_id);
                }
                Err(err) => {
                    had_failures = true;
                    blocked_slots.insert(pane.slot_id);
                    tracing::warn!(
                        run_id = %pane.run_id,
                        slot_id = pane.slot_id,
                        error = %format!("{err:#}"),
                        "could not classify hosted viewer; skipping this slot and continuing"
                    );
                }
            }
        }
        Ok(WorkerViewerReconcile {
            live,
            blocked_slots,
            had_failures,
        })
    }

    pub(super) async fn attach_worker_viewer(
        &self,
        input: AttachWorkerPaneInput,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        let _guard = self.attach_pane_lock.lock().await;
        let request = EngineToAppRequest::AttachWorkerPane(input.clone());
        let response = self.send_to_app(request.clone(), timeout).await?;
        if let EngineToAppResponse::AttachWorkerPane {
            result: Err(EngineToAppError::SlotBusy { occupying_run_id }),
        } = &response
        {
            if occupying_run_id.as_deref() == Some(input.run_id.as_str()) {
                return Ok(EngineToAppResponse::AttachWorkerPane {
                    result: Ok(AttachWorkerPaneResult {}),
                });
            }
            // A different live run is an ownership conflict, never permission
            // to evict it. Unknown identity also cannot authorize a detach.
            let stale = occupying_run_id
                .as_deref()
                .is_some_and(|occupant| occupant != input.run_id && self.viewer_is_stale(occupant).unwrap_or(false));
            if stale && self.detach_viewer(input.slot_id).await.is_ok() {
                let retry = self.send_to_app(request, timeout).await;
                if matches!(retry, Ok(EngineToAppResponse::AttachWorkerPane { result: Ok(_) })) {
                    return retry;
                }
                tracing::error!(run_id = %input.run_id, ?retry, "worker viewer attach retry failed after SlotBusy");
            }
            // Preserve the typed rejection even if detach/retry loses the app
            // connection. Spawn must record a failure, not become headless.
            // Attention is filed by the spawn coordinator, not here: this
            // helper is also used to reattach already-running workers.
            tracing::error!(run_id = %input.run_id, slot_id = input.slot_id, ?occupying_run_id,
                "worker viewer SlotBusy could not be reconciled");
        }
        Ok(response)
    }
}

#[async_trait]
impl crate::pool_claim_sweep::WorkerViewerDetach for ServerState {
    async fn confirm_process_torn_down(&self, execution_id: &str) -> Result<(), String> {
        match self.reap_tmux_worker(execution_id).await {
            tmux_teardown::TmuxTeardownOutcome::Reaped => Ok(()),
            outcome => Err(format!("tmux teardown unconfirmed: {outcome:?}")),
        }
    }

    async fn confirm_viewers_detached(&self, run_ids: &[String]) -> Vec<Result<(), String>> {
        if run_ids.is_empty() {
            return Vec::new();
        }
        let wanted: HashSet<&str> = run_ids.iter().map(String::as_str).collect();
        let _guard = self.attach_pane_lock.lock().await;
        let panes = match self.hosted_worker_viewers().await {
            Ok(Some(panes)) => panes,
            Ok(None) => return run_ids.iter().map(|_| Ok(())).collect(),
            Err(err) => return run_ids.iter().map(|_| Err(err.clone())).collect(),
        };
        let mut failed: HashMap<String, String> = HashMap::new();
        for pane in panes {
            if !wanted.contains(pane.run_id.as_str()) {
                continue;
            }
            if let Err(err) = self.detach_viewer(pane.slot_id).await {
                failed.insert(pane.run_id, err);
            }
        }
        run_ids
            .iter()
            .map(|id| match failed.get(id) {
                Some(err) => Err(err.clone()),
                None => Ok(()),
            })
            .collect()
    }
}
