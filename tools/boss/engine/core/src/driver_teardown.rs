//! Best-effort driver-workspace teardown, called from every execution
//! termination path (normal completion, stop, reap, orphaned/husk recovery,
//! app-crash reconciliation).
//!
//! Mirrors [`crate::driver::AgentDriver::provision_workspace`] with
//! [`crate::driver::AgentDriver::teardown_workspace`]: any driver that
//! creates per-run state outside the cube workspace (a per-worker config
//! dir, a cache dir, a socket, a temp credential file) gets a chance to
//! clean it up wherever the engine considers a run over.
//!
//! There is currently no per-execution driver slug stored anywhere in the
//! DB (only `product.default_driver` / `task.driver` feed spawn-time
//! resolution via `resolve_spawn_config`); every registered driver besides
//! Claude is future work. Matching the existing precedent elsewhere in the
//! engine (`automation_triage::resolve_triage_decision` call sites), this
//! hardcodes [`crate::driver::ClaudeDriver`] until a driver slug is threaded
//! onto executions.

use std::path::Path;

use crate::driver::{AgentDriver, ClaudeDriver};

/// Tear down driver-owned, out-of-workspace state for a terminated
/// execution. Never fails the caller: a teardown error is logged and
/// swallowed, since cleanup must not turn an otherwise-successful run into a
/// failure.
pub async fn teardown_driver_workspace(execution_id: &str, workspace_path: &Path) {
    if let Err(err) = ClaudeDriver.teardown_workspace(workspace_path, execution_id).await {
        tracing::warn!(
            execution_id,
            workspace_path = %workspace_path.display(),
            error = %format!("{err:#}"),
            "driver workspace teardown failed (non-fatal)",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_for_claude() {
        let dir = tempfile::tempdir().unwrap();
        // No-op driver, no panic, nothing propagated — just confirm it runs.
        teardown_driver_workspace("exec-1", dir.path()).await;
    }
}
