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
//! The cleanup handle is the opaque [`boss_protocol::DriverRuntimeState`]
//! the driver returned from provision, reloaded from the execution row —
//! never inferred from the engine environment or by scanning a shared
//! provider home. The driver itself is resolved via the registry from
//! the same `tasks.driver` → `products.default_driver` → engine-default
//! precedence used at spawn time ([`WorkDb::get_execution_driver_slug`]).

use std::path::Path;

use crate::driver::DriverRegistry;
use crate::work::WorkDb;

/// Tear down driver-owned, out-of-workspace state for a terminated
/// execution. Never fails the caller: a teardown error is logged and
/// swallowed, since cleanup must not turn an otherwise-successful run into a
/// failure.
///
/// Loads the persisted [`boss_protocol::DriverRuntimeState`] (if any) and
/// the resolved driver slug from `work_db`, then hands both to the
/// registry-resolved driver. `workspace_path` is `None` when the
/// execution's workspace path was never recorded or was already cleared
/// by a racing teardown — callers must still call this unconditionally
/// in that case, since a driver may key its out-of-workspace state by
/// the recorded runtime state alone (e.g. a per-worker `CODEX_HOME`).
pub async fn teardown_driver_workspace(work_db: &WorkDb, execution_id: &str, workspace_path: Option<&Path>) {
    #[cfg(test)]
    test_hooks::record_call();

    let runtime_state = match work_db.get_driver_runtime_state(execution_id) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver workspace teardown: failed to load driver_runtime_state (non-fatal)",
            );
            None
        }
    };

    let driver_slug = match work_db.get_execution_driver_slug(execution_id) {
        Ok(Some(slug)) => slug,
        Ok(None) => {
            // Unknown execution or no tasks row: nothing to resolve. A
            // missing slug with no runtime state is a pure no-op; a missing
            // slug *with* runtime state is a data-integrity problem we log.
            if runtime_state.is_some() {
                tracing::warn!(
                    execution_id,
                    "driver workspace teardown: runtime state present but no driver slug \
                     could be resolved; skipping teardown rather than inventing a home"
                );
            }
            return;
        }
        Err(err) => {
            tracing::warn!(
                execution_id,
                error = %format!("{err:#}"),
                "driver workspace teardown: failed to resolve driver slug (non-fatal)",
            );
            return;
        }
    };

    let registry = DriverRegistry::default();
    let driver = match registry.get(&driver_slug) {
        Some(d) => d.clone(),
        None => {
            tracing::warn!(
                execution_id,
                driver = %driver_slug,
                "driver workspace teardown: unknown driver slug; skipping"
            );
            return;
        }
    };

    if let Err(err) = driver
        .teardown_workspace(workspace_path, execution_id, runtime_state.as_ref())
        .await
    {
        tracing::warn!(
            execution_id,
            driver = %driver_slug,
            workspace_path = ?workspace_path.map(Path::display),
            has_runtime_state = runtime_state.is_some(),
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
    use crate::test_support::{create_ready_chore_execution, create_test_chore, create_test_product, open_db};
    use boss_protocol::DriverRuntimeState;

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_for_claude() {
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        let dir = tempfile::tempdir().unwrap();
        // No-op driver, no panic, nothing propagated — just confirm it runs.
        teardown_driver_workspace(&db, &execution.id, Some(dir.path())).await;
    }

    #[tokio::test]
    async fn teardown_driver_workspace_succeeds_with_no_path() {
        // Callers must invoke teardown even when the workspace path is
        // unknown (never recorded, or cleared by a racing teardown) — a
        // driver may key its out-of-workspace state by the recorded
        // runtime state alone.
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);
        teardown_driver_workspace(&db, &execution.id, None).await;
    }

    #[tokio::test]
    async fn teardown_loads_persisted_runtime_state_without_inferring() {
        // Contract: teardown reloads the opaque state from the execution
        // row and never invents a cleanup target. Claude ignores the
        // state (no-op); a future Codex driver would use the recorded
        // path rather than scanning a shared provider home.
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        let state = DriverRuntimeState::new(serde_json::json!({
            "codex_home": "/tmp/boss-codex-homes/exec-test"
        }));
        db.set_driver_runtime_state(&execution.id, Some(&state)).unwrap();

        // Round-trip through the dedicated getter (same path teardown uses).
        let loaded = db.get_driver_runtime_state(&execution.id).unwrap();
        assert_eq!(loaded.as_ref().map(|s| s.as_value()), Some(state.as_value()));

        // Teardown must not panic or fail the caller even with state present.
        teardown_driver_workspace(&db, &execution.id, None).await;

        // State survives teardown (idempotent; retention may still need it).
        let still = db.get_driver_runtime_state(&execution.id).unwrap();
        assert_eq!(still.as_ref().map(|s| s.as_value()), Some(state.as_value()));
    }

    #[test]
    fn clear_execution_workspace_preserves_driver_runtime_state() {
        // Workspace release must not drop the cleanup handle — future
        // Codex retention operates only on a recorded root.
        let (_dir, db) = open_db();
        let product = create_test_product(&db);
        let chore = create_test_chore(&db, &product.id, "test chore");
        let execution = create_ready_chore_execution(&db, &chore.id);

        // Simulate a leased execution with a recorded runtime state.
        {
            let conn = db.connect().unwrap();
            conn.execute(
                "UPDATE work_executions
                 SET cube_lease_id = 'lease-1',
                     cube_workspace_id = 'ws-1',
                     workspace_path = '/tmp/ws',
                     status = 'running'
                 WHERE id = ?1",
                [&execution.id],
            )
            .unwrap();
        }
        let state = DriverRuntimeState::new(serde_json::json!({"codex_home": "/tmp/codex-home"}));
        db.set_driver_runtime_state(&execution.id, Some(&state)).unwrap();

        let cleared = db.clear_execution_workspace(&execution.id).unwrap();
        assert!(cleared.is_some());

        let reloaded = db.get_execution(&execution.id).unwrap();
        assert!(reloaded.workspace_path.is_none());
        assert_eq!(
            reloaded.driver_runtime_state.as_ref().map(|s| s.as_value()),
            Some(state.as_value()),
        );
    }
}
