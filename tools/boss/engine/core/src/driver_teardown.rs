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
///
/// `workspace_path` is `None` when the execution's workspace path was never
/// recorded or was already cleared by a racing teardown — callers must
/// still call this unconditionally in that case, since a driver may key its
/// out-of-workspace state by `execution_id` alone (e.g. a per-worker
/// `CODEX_HOME`), not by workspace path.
pub async fn teardown_driver_workspace(execution_id: &str, workspace_path: Option<&Path>) {
    #[cfg(test)]
    test_hooks::record_call();
    if let Err(err) = ClaudeDriver.teardown_workspace(workspace_path, execution_id).await {
        tracing::warn!(
            execution_id,
            workspace_path = ?workspace_path.map(Path::display),
            error = %format!("{err:#}"),
            "driver workspace teardown failed (non-fatal)",
        );
    }
}

/// Test-only call counter for [`teardown_driver_workspace`] — the entry
/// point every one of the ~15 termination-path call sites actually invokes
/// (they cannot inject a driver). A `thread_local`, not a shared atomic:
/// `#[tokio::test]` defaults to the `current_thread` runtime flavor, so each
/// test's async work — including everything a call site under test
/// transitively awaits — stays on that test's own OS thread, and the
/// counter never sees another, unrelated test's calls running in parallel.
#[cfg(test)]
pub(crate) mod test_hooks {
    use std::cell::Cell;

    thread_local! {
        static CALL_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_call() {
        CALL_COUNT.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn reset() {
        CALL_COUNT.with(|c| c.set(0));
    }

    pub(crate) fn count() -> usize {
        CALL_COUNT.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_for_claude() {
        let dir = tempfile::tempdir().unwrap();
        // No-op driver, no panic, nothing propagated — just confirm it runs.
        teardown_driver_workspace("exec-1", Some(dir.path())).await;
    }

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_with_no_path() {
        // Callers must invoke teardown even when the workspace path is
        // unknown (never recorded, or cleared by a racing teardown) — a
        // driver may key its out-of-workspace state by run id alone.
        teardown_driver_workspace("exec-1", None).await;
    }
}
