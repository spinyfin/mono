//! Check the CLI's effective permission posture before admitting input.

use super::*;
use boss_engine_driver::SettingsLogCheck;

pub(super) fn check(record: &ExecutionRecord) -> Result<bool> {
    let registry = DriverRegistry::default();
    let Some(driver) = registry.get(&record.driver_id) else {
        return Ok(true);
    };
    let Some(spec) = driver.settings_log_check() else {
        return Ok(true);
    };
    let state = record
        .runtime_state
        .as_ref()
        .context("settings-log admission requires runtime state")?;
    Ok((spec.verify)(state)? == SettingsLogCheck::ConsistentSoFar)
}

pub(super) fn require_ready(record: &ExecutionRecord) -> Result<()> {
    ensure!(check(record)?, "settings have not initialized; input refused");
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
