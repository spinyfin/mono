//! Check the CLI's effective permission posture before admitting input.

use super::*;
use boss_engine_driver::agy::{
    AgyRuntimeState,
    permissions::{SettingsLogCheck, verify_settings_log_files},
};

pub(super) fn check(record: &ExecutionRecord) -> Result<bool> {
    if record.driver_id != "agy" {
        return Ok(true);
    }
    let state = AgyRuntimeState::from_driver_runtime_state(
        record
            .runtime_state
            .as_ref()
            .context("agy permission check requires runtime state")?,
    )?;
    Ok(verify_settings_log_files(&state.session_settings_path, &state.log_file)? == SettingsLogCheck::ConsistentSoFar)
}

pub(super) fn require_ready(record: &ExecutionRecord) -> Result<()> {
    ensure!(check(record)?, "agy settings have not initialized; input refused");
    Ok(())
}

impl<T: CubeJsonTransport, S: ExecutionStore> Supervisor<T, S> {
    pub(super) async fn permission_failed(
        &self,
        command: &Command,
        mut record: ExecutionRecord,
        error: anyhow::Error,
    ) -> Result<CommandResult> {
        // Fail closed, and retire only after the token-verified kill succeeds.
        self.tmux
            .kill_session_verified(&record.session, &record.spawn_token)
            .await?;
        let pane = self.observe(&record).await;
        self.settle(&mut record, &pane).await?;
        Ok(outcome(
            command,
            ResultCode::Rejected,
            format!("permission verification failed: {error:#}"),
            Some(pane),
        ))
    }
}
