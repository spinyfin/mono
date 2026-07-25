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
    teardown_driver_workspace_with(&ClaudeDriver, execution_id, workspace_path).await;
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

/// Driver-injectable version of [`teardown_driver_workspace`], so wiring
/// call sites can be tested against a recording fake instead of the
/// hardcoded `ClaudeDriver`.
pub async fn teardown_driver_workspace_with(
    driver: &dyn AgentDriver,
    execution_id: &str,
    workspace_path: Option<&Path>,
) {
    if let Err(err) = driver.teardown_workspace(workspace_path, execution_id).await {
        tracing::warn!(
            execution_id,
            workspace_path = ?workspace_path.map(Path::display),
            error = %format!("{err:#}"),
            "driver workspace teardown failed (non-fatal)",
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use boss_engine_structured_output::StructuredOutputKind;
    use boss_engine_structured_output::fallback::FallbackCandidate;
    use boss_protocol::{NormalizeError, WorkerEvent};

    use super::*;
    use crate::driver::{
        CapabilitySet, DriverDescriptor, ModelMenu, ProgressFidelity, ProgressObservationConfig,
        ProgressObservationWiring, ToolUseInterceptionConfig, ToolUseInterceptionWiring, WorkerErrorClass,
    };

    /// Records every `teardown_workspace` call so wiring call sites (spread
    /// across ~15 termination paths) can assert teardown actually fired,
    /// instead of only exercising the no-op `ClaudeDriver` impl.
    #[derive(Default)]
    pub(crate) struct RecordingDriver {
        pub(crate) calls: AtomicUsize,
    }

    static RECORDING_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
        name: "recording-test-driver",
        label: "Recording Test Driver",
        binary: "recording-test-driver",
        config_dir: ".recording-test-driver",
        agent_rules_filename: "AGENTS.md",
        initial_prompt_filename: "initial-prompt.txt",
        model_menu: ModelMenu {
            engine_default: "test-model",
            effort_value_for_level: |_| None,
            default_model_for_level: |_| "test-model",
            prompt_addendum_for_level: |_| None,
            model_requires_auto_permissions: |_| false,
        },
    };

    #[async_trait]
    impl AgentDriver for RecordingDriver {
        fn descriptor(&self) -> &DriverDescriptor {
            &RECORDING_DESCRIPTOR
        }
        fn capabilities(&self) -> CapabilitySet {
            CapabilitySet::new([])
        }
        fn spawn_invocation(
            &self,
            _model: &str,
            _claude_effort: Option<&str>,
            _prompt_addendum: Option<&Path>,
            _non_opus_auto_mode: bool,
            _permission_mode_override: Option<&str>,
        ) -> String {
            unimplemented!()
        }
        async fn provision_workspace(
            &self,
            _workspace: &Path,
            _prompt_text: &str,
            _run_id: &str,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn teardown_workspace(&self, _workspace: Option<&Path>, _run_id: &str) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn write_permission_config(
            &self,
            _input: &crate::driver::PermissionInput,
            _dest_dir: &Path,
        ) -> anyhow::Result<crate::driver::PermissionArtifacts> {
            unimplemented!()
        }
        fn progress_fidelity(&self) -> ProgressFidelity {
            unimplemented!()
        }
        fn progress_observation_wiring(&self, _config: &ProgressObservationConfig) -> ProgressObservationWiring {
            unimplemented!()
        }
        fn normalize_progress_event(&self, _raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
            unimplemented!()
        }
        fn tool_use_interception_wiring(&self, _config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
            unimplemented!()
        }
        fn agent_rules_preamble(&self) -> &'static str {
            unimplemented!()
        }
        fn transcript_path_for_session(&self, _raw: &serde_json::Value) -> Option<String> {
            None
        }
        fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
            raw
        }
        fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
            None
        }
        fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
            unimplemented!()
        }
        fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
            Vec::new()
        }
    }

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

    #[tokio::test]
    async fn teardown_driver_workspace_with_invokes_the_injected_driver() {
        let driver = RecordingDriver::default();
        teardown_driver_workspace_with(&driver, "exec-1", None).await;
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
    }
}
