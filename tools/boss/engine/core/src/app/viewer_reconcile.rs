//! Engine-owned reconciliation of app presentation against execution truth.

use super::*;
use boss_protocol::{AttachWorkerPaneInput, HostedPaneEntry};

impl ServerState {
    fn clear_viewer_failure(&self, run_id: &str) {
        if let Err(err) = self
            .work_db
            .resolve_attention_kind_for_execution(run_id, crate::coordinator::PANE_SPAWN_FAILED_ATTENTION_KIND)
        {
            tracing::error!(run_id, %err, "could not resolve worker viewer failure");
        }
    }

    pub(super) async fn hosted_worker_viewers(&self) -> Result<Vec<HostedPaneEntry>, String> {
        match self
            .send_to_app(
                EngineToAppRequest::ListHostedPanes(ListHostedPanesInput {}),
                Duration::from_secs(5),
            )
            .await
        {
            Ok(EngineToAppResponse::ListHostedPanes { result: Ok(result) }) => Ok(result.panes),
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
    pub(super) async fn reconcile_worker_viewers(&self) -> Result<HashSet<String>, String> {
        let _guard = self.attach_pane_lock.lock().await;
        let mut live = HashSet::new();
        for pane in self.hosted_worker_viewers().await? {
            if self.viewer_is_stale(&pane.run_id).map_err(|err| format!("{err:#}"))? {
                self.detach_viewer(pane.slot_id).await?;
                tracing::info!(run_id = %pane.run_id, slot_id = pane.slot_id, "detached stale app viewer");
            } else {
                live.insert(pane.run_id);
            }
        }
        Ok(live)
    }

    pub(super) async fn attach_worker_viewer(
        &self,
        input: AttachWorkerPaneInput,
        timeout: Duration,
    ) -> Result<EngineToAppResponse, SendToAppError> {
        let _guard = self.attach_pane_lock.lock().await;
        let request = EngineToAppRequest::AttachWorkerPane(input.clone());
        let response = self.send_to_app(request.clone(), timeout).await?;
        if matches!(response, EngineToAppResponse::AttachWorkerPane { result: Ok(_) }) {
            self.clear_viewer_failure(&input.run_id);
        }
        if let EngineToAppResponse::AttachWorkerPane {
            result: Err(EngineToAppError::SlotBusy { occupying_run_id }),
        } = &response
        {
            // A different live run is an ownership conflict, never permission
            // to evict it. Unknown identity also cannot authorize a detach.
            let stale = occupying_run_id
                .as_deref()
                .is_some_and(|occupant| occupant != input.run_id && self.viewer_is_stale(occupant).unwrap_or(false));
            if stale && self.detach_viewer(input.slot_id).await.is_ok() {
                let retry = self.send_to_app(request, timeout).await;
                if matches!(retry, Ok(EngineToAppResponse::AttachWorkerPane { result: Ok(_) })) {
                    self.clear_viewer_failure(&input.run_id);
                    return retry;
                }
                tracing::error!(run_id = %input.run_id, ?retry, "worker viewer attach retry failed after SlotBusy");
            }
            // Preserve the typed rejection even if detach/retry loses the app
            // connection. Spawn must record a failure, not become headless.
            tracing::error!(run_id = %input.run_id, slot_id = input.slot_id, ?occupying_run_id,
                "worker viewer SlotBusy could not be reconciled");
            if let Err(err) = self.work_db.create_attention_item(boss_protocol::CreateAttentionItemInput {
                execution_id: Some(input.run_id.clone()),
                work_item_id: None,
                kind: crate::coordinator::PANE_SPAWN_FAILED_ATTENTION_KIND.into(),
                status: None,
                title: "Worker viewer could not attach".into(),
                body_markdown: format!("Slot {} rejected run {} with SlotBusy; occupant: {:?}. Stale-viewer recovery did not establish a viewer.", input.slot_id, input.run_id, occupying_run_id),
                resolved_at: None,
            }) {
                tracing::error!(%err, "could not record worker viewer conflict attention");
            }
        }
        Ok(response)
    }
}

#[async_trait]
impl crate::pool_claim_sweep::WorkerViewerDetach for ServerState {
    async fn confirm_viewer_detached(&self, run_id: &str) -> Result<(), String> {
        let _guard = self.attach_pane_lock.lock().await;
        for pane in self.hosted_worker_viewers().await? {
            if pane.run_id == run_id {
                self.detach_viewer(pane.slot_id).await?;
            }
        }
        Ok(())
    }
}
