//! Driver provisioning and recorded input-home reclamation, without work DB policy.
use anyhow::{Context, Result};
use boss_engine_driver::AgentDriver;
use boss_protocol::DriverRuntimeState;
use std::path::Path;

/// Driver provisioning owns the initial-prompt relay and returns its cleanup handle.
/// Validate bytes before any driver filesystem side effect.
pub async fn provision(
    driver: &dyn AgentDriver,
    workspace: &Path,
    prompt_utf8: &[u8],
    run_id: &str,
) -> Result<Option<DriverRuntimeState>> {
    let prompt = std::str::from_utf8(prompt_utf8).context("initial prompt must be UTF-8")?;
    driver.provision_workspace(workspace, prompt, run_id).await
}

/// Reclaim only the home recorded by provisioning, after the caller has retained
/// any required transcript evidence. Never infer ownership from a workspace path.
pub fn reclaim(state: &DriverRuntimeState) -> Result<()> {
    if let Some(home) = boss_engine_driver::initial_prompt::owned_home_from_runtime_state(state) {
        reclaim_home(&home)?;
    }
    Ok(())
}

/// Reclaim an already recorded home after hub retention authorizes it.
pub fn reclaim_home(home: &Path) -> Result<()> {
    boss_engine_driver::codex::reclaim_codex_home(home)
}
