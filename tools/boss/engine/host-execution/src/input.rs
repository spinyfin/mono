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
    kind: boss_engine_driver::ExecutionWorkspaceKind,
) -> Result<Option<DriverRuntimeState>> {
    let prompt = std::str::from_utf8(prompt_utf8).context("initial prompt must be UTF-8")?;
    driver
        .provision_workspace_with_kind(workspace, prompt, run_id, kind)
        .await
}

/// Reclaim only the home recorded by provisioning, after the caller has retained
/// any required transcript evidence. Never infer ownership from a workspace path.
pub fn reclaim(state: &DriverRuntimeState) -> Result<()> {
    if let Some(grok) = grok_state(state) {
        // The container holds both the Grok home and the scoped process home.
        if let Some(container) = grok.grok_home.parent() {
            boss_engine_driver::grok::reclaim_grok_home(container)?;
        }
        return Ok(());
    }
    if let Some(home) = boss_engine_driver::initial_prompt::owned_home_from_runtime_state(state) {
        reclaim_home(&home)?;
    }
    Ok(())
}

fn grok_state(state: &DriverRuntimeState) -> Option<boss_engine_driver::grok::GrokRuntimeState> {
    state.as_value().get("grok_home")?;
    boss_engine_driver::grok::GrokRuntimeState::from_driver_runtime_state(state).ok()
}

/// The per-run home a driver's runtime state owns and holds the delivery
/// markers: Codex and Claude via the shared rule, plus a Grok home.
pub fn owned_home(state: &DriverRuntimeState) -> Option<std::path::PathBuf> {
    boss_engine_driver::initial_prompt::owned_home_from_runtime_state(state)
        .or_else(|| grok_state(state).map(|grok| grok.grok_home))
}

/// Reclaim an already recorded home after hub retention authorizes it.
pub fn reclaim_home(home: &Path) -> Result<()> {
    boss_engine_driver::codex::reclaim_codex_home(home)
}
