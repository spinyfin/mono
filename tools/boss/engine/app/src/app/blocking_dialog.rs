//! Driver policy and durable failure for refused pane input.
use super::*;
use boss_tmux::Tmux;

impl ServerState {
    pub(super) async fn settle_blocked_probe(&self, run_id: &str, probe_id: &str, detail: String) {
        self.take_in_flight_probe_if(run_id, probe_id, |_| true);
        self.set_probe_lifecycle_detail(
            probe_id,
            boss_protocol::ProbeDeliveryState::Abandoned,
            Some(detail.clone()),
        );
        self.surface_undelivered_probe(run_id, probe_id, boss_protocol::ProbeDeliveryState::Abandoned, &detail)
            .await;
    }

    pub(super) fn guard_pane_input(&self, run_id: &str, tmux: Tmux) -> anyhow::Result<Tmux> {
        anyhow::ensure!(
            !self.work_db.get_execution(run_id)?.status.is_terminal(),
            "execution is terminal; pane input refused"
        );
        let driver = self.pane_input_driver(run_id)?;
        Ok(tmux.with_blocking_dialog_markers(driver.blocking_dialog_markers()))
    }

    pub(super) fn pane_input_driver(&self, run_id: &str) -> anyhow::Result<Arc<dyn boss_engine_driver::AgentDriver>> {
        boss_engine_completion::driver_transcript::driver_for_spawned_execution(&self.work_db, run_id)
            .context("no spawned driver recorded for pane input")
    }

    pub(super) async fn settle_spoke_dialog_failure(
        &self,
        run_id: &str,
        outcome: &boss_execution_contracts::ControlOutcome,
    ) {
        if let boss_execution_contracts::ControlOutcome::Rejected { reason, .. } = outcome
            && boss_tmux::BlockingDialogOnScreen::is_delivery_rejection(reason)
        {
            if let Err(error) = self.completion_handler.fail_blocking_dialog(run_id, reason).await {
                tracing::error!(run_id, %error, "could not finalize spoke blocking-dialog failure");
            }
            let _ = self.release_worker_pane(run_id).await;
        }
    }

    pub(super) async fn settle_pane_input_failure(&self, run_id: &str, result: &anyhow::Result<()>) {
        if let Err(error) = result
            && error.downcast_ref::<boss_tmux::BlockingDialogOnScreen>().is_some()
        {
            if let Err(failure) = self
                .completion_handler
                .fail_blocking_dialog(run_id, &error.to_string())
                .await
            {
                tracing::error!(run_id, %failure, %error, "could not finalize blocking-dialog failure");
            }
            let release = self.release_worker_pane(run_id).await;
            tracing::error!(run_id, ?release, %error, "blocked worker pane released");
        }
    }
}

#[async_trait]
impl boss_engine_runtime::spawn_ack_sweep::SpawnAckReaper for ServerState {
    async fn resubmit_prompt(&self, execution_id: &str) -> anyhow::Result<()> {
        let identity = self
            .work_db
            .tmux_identity_for_execution(execution_id)?
            .ok_or_else(|| anyhow::anyhow!("no recorded tmux identity"))?;
        let tmux = self.tmux_for_pane_delivery(execution_id)?;
        let token = boss_engine_runtime::tmux_adoption::session_spawn_token(&tmux, &identity.session_name).await?;
        anyhow::ensure!(
            token.as_deref() == Some(identity.spawn_token.as_str()),
            "worker session token changed"
        );
        anyhow::ensure!(
            !self.work_db.get_execution(execution_id)?.status.is_terminal(),
            "execution is terminal"
        );
        let tmux = self.guard_pane_input(execution_id, tmux)?;
        let result = tmux.send_submit(&identity.session_name).await;
        self.settle_pane_input_failure(execution_id, &result).await;
        result
    }

    /// Route the driver-start-timeout reap through the same
    /// `release_worker_pane` tmux teardown as the stale-worker sweep:
    /// verifies and kills the recorded tmux session, signals the
    /// recorded pane pid's process group, releases the pool slot, and
    /// drops the live-state entry.
    async fn reap_worker(&self, execution_id: &str) {
        let _ = ServerState::release_worker_pane(self, execution_id).await;
    }
}
