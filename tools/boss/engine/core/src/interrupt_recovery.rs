//! Engine-owned executor for [`crate::driver::AgentDriver::prepare_interrupt_recovery`]
//! (design T-12: turn-end recovery for Esc-cancelled Grok turns).
//!
//! A driver whose interrupt path does not produce a normal turn boundary
//! (Grok: Esc-cancelled turns skip the `Stop` hook entirely) returns a
//! [`crate::driver::InterruptRecoverySnapshot`] from `prepare_interrupt_recovery`,
//! captured *before* the engine delivers the interrupt. [`run_interrupt_recovery`]
//! is the bounded tail-with-fallback loop that consumes it: poll the named
//! file for new complete lines, ask the driver whether any of them is its
//! turn-end evidence, and — on a match, or once the settle window elapses —
//! feed a synthetic hook-shaped `WorkerEvent::Stop` through the exact same
//! [`crate::events_socket::IncomingHookEvent::resolve`] /
//! [`WorkerEventSink::dispatch_worker_event`] path every other progress
//! ingress uses. This is deliberately the *only* thing this module does with
//! `events.jsonl` — it is not adopted as a general progress transport, and
//! this executor is not started except in the narrow window right after an
//! interrupt (see the call site in `app/pane_ops.rs`).
//!
//! Observed vs. fallback are logged at different levels so an operator can
//! tell a real cancellation record apart from the sanctioned "no evidence
//! within the settle window, unstick the slot anyway" path — the latter is
//! expected to fire occasionally (see the design's Esc-cancel hazard
//! writeup) but must stay visible when it does.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use boss_protocol::{StopReason, WorkerEvent};

use crate::driver::{AgentDriver, InterruptRecoverySnapshot};
use crate::events_socket::IncomingHookEvent;
use crate::stdout_progress::WorkerEventSink;

/// How often the tail polls the file for new content. Small relative to the
/// settle window so an observed cancellation is picked up promptly rather
/// than waiting out most of the window.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Whether the recovered turn end was actually observed in the driver's
/// evidence file, or synthesized after the settle window elapsed with none
/// found. Returned so callers (and tests) can distinguish the two without
/// depending on log output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptRecoveryOutcome {
    /// The driver recognised a turn-end record within the settle window.
    Observed,
    /// No matching record appeared before the settle window elapsed; a
    /// turn end was synthesized so the slot does not pin at `Working`
    /// forever. Sanctioned and expected to happen occasionally.
    Fallback,
}

/// Run the bounded interrupt-recovery observation for `run_id` and dispatch
/// its result through `sink`'s normal fan-out.
///
/// Intended to be spawned as a detached background task immediately after
/// the engine confirms the interrupt was delivered (see `app/pane_ops.rs`);
/// it does not block the RPC that triggered it. `driver` must be the same
/// driver instance `prepare_interrupt_recovery` was called on, since the
/// synthetic event is decoded through it exactly as a real hook payload
/// would be. `sink` is generic over [`WorkerEventSink`] (the engine passes
/// its `Arc<ServerState>`, which implements it) rather than a trait object,
/// matching how every other JSONL ingress in this crate takes its sink.
pub async fn run_interrupt_recovery<S>(
    driver: Arc<dyn AgentDriver>,
    run_id: String,
    snapshot: InterruptRecoverySnapshot,
    sink: S,
) -> InterruptRecoveryOutcome
where
    S: WorkerEventSink + 'static,
{
    let observed = poll_for_turn_end(driver.as_ref(), &snapshot).await;
    let outcome = if observed {
        InterruptRecoveryOutcome::Observed
    } else {
        InterruptRecoveryOutcome::Fallback
    };
    match outcome {
        InterruptRecoveryOutcome::Observed => {
            tracing::info!(
                run_id,
                events_path = %snapshot.events_path.display(),
                "interrupt recovery: observed cancelled turn-end record",
            );
        }
        InterruptRecoveryOutcome::Fallback => {
            tracing::warn!(
                run_id,
                events_path = %snapshot.events_path.display(),
                settle_window_secs = snapshot.settle_window.as_secs_f64(),
                "interrupt recovery: settle window elapsed with no cancellation record; \
                 synthesizing turn end so the slot does not pin at Working forever",
            );
        }
    }
    let event = WorkerEvent::Stop {
        session_id: snapshot.session_id.clone(),
        stop_hook_active: false,
        stop_reason: StopReason::Interrupted,
    };
    let incoming = IncomingHookEvent::resolve(driver.as_ref(), event, Some(run_id), None, None);
    sink.dispatch_worker_event(incoming).await;
    outcome
}

/// Poll `snapshot.events_path` from `snapshot.offset` until `driver`
/// recognises a turn-end record among the new complete lines, or the
/// settle window elapses. Returns `true` iff a record was recognised.
async fn poll_for_turn_end(driver: &dyn AgentDriver, snapshot: &InterruptRecoverySnapshot) -> bool {
    let deadline = tokio::time::Instant::now() + snapshot.settle_window;
    let mut offset = snapshot.offset;
    loop {
        let (lines, new_offset) = read_new_complete_lines(&snapshot.events_path, offset).await;
        offset = new_offset;
        for line in &lines {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if driver.is_interrupt_recovery_turn_end(&value) {
                return true;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(tokio::time::Instant::now()))).await;
    }
}

/// Read whatever complete (`\n`-terminated) lines have been appended to
/// `path` since `offset`, returning them plus the byte offset just past the
/// last complete line consumed. A trailing partial line (the writer hasn't
/// flushed its terminating newline yet) is left unconsumed so it is read
/// again, complete, on a later poll.
///
/// Tolerant of the file not existing yet (an interrupt delivered before the
/// session has written anything) and of the file being shorter than
/// `offset` (should not happen in practice for an append-only session log,
/// but resetting to a fresh read is safer than seeking past EOF).
async fn read_new_complete_lines(path: &Path, offset: u64) -> (Vec<String>, u64) {
    let Ok(contents) = tokio::fs::read(path).await else {
        return (Vec::new(), offset);
    };
    let start = if (offset as usize) <= contents.len() {
        offset as usize
    } else {
        0
    };
    let new_bytes = &contents[start..];
    let Some(last_newline) = new_bytes.iter().rposition(|&b| b == b'\n') else {
        return (Vec::new(), start as u64);
    };
    let complete = &new_bytes[..=last_newline];
    let lines = String::from_utf8_lossy(complete).lines().map(str::to_owned).collect();
    (lines, (start + last_newline + 1) as u64)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use boss_protocol::{NormalizeError, PaneMonitorSpec};
    use tempfile::TempDir;

    use super::*;
    use crate::driver::{
        AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, ModelMenu, PermissionArtifacts,
        PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig, ToolUseInterceptionConfig,
        ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass,
    };
    use crate::events_socket::IncomingHookEvent;

    static TEST_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
        name: "test-recovery-driver",
        label: "Test Recovery Driver",
        binary: "test-recovery-driver",
        config_dir: ".test",
        agent_rules_filename: "AGENTS.md",
        initial_prompt_filename: "initial-prompt.txt",
        model_menu: ModelMenu {
            engine_default: "test-model",
            effort_value_for_level: |_| None,
            default_model_for_level: |_| "test-model",
            model_for_reasoning: |_| "test-model",
            review_model_for_tier: |_| "test-model",
            design_investigation_model: None,
            prompt_addendum_for_level: |_| None,
            model_requires_auto_permissions: |_| false,
            model_belongs_to_driver: |_| true,
        },
    };

    /// Minimal driver stub for exercising [`poll_for_turn_end`] /
    /// [`run_interrupt_recovery`] without pulling in a real Grok/Claude
    /// implementation. Recognises the same `{"cancelled": true}` shape a
    /// test fixture line uses — the exact JSON schema is Grok's concern
    /// (covered in `boss_engine_driver::grok::turn_end_recovery`'s own
    /// tests), not this engine-side executor's.
    #[derive(Default)]
    struct FakeRecoveryDriver;

    #[async_trait]
    impl AgentDriver for FakeRecoveryDriver {
        fn descriptor(&self) -> &DriverDescriptor {
            &TEST_DESCRIPTOR
        }
        fn capabilities(&self) -> CapabilitySet {
            CapabilitySet::new([Capability::ControlVerbs])
        }
        fn spawn_invocation(&self, _request: crate::driver::SpawnRequest<'_>) -> crate::driver::SpawnPlan {
            unimplemented!("not exercised by these tests")
        }
        async fn provision_workspace(
            &self,
            _workspace: &std::path::Path,
            _prompt_text: &str,
            _run_id: &str,
        ) -> anyhow::Result<Option<DriverRuntimeState>> {
            Ok(None)
        }
        async fn teardown_workspace(
            &self,
            _workspace: Option<&std::path::Path>,
            _run_id: &str,
            _runtime_state: Option<&DriverRuntimeState>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn write_permission_config(
            &self,
            _input: &PermissionInput,
            _dest_dir: &std::path::Path,
        ) -> anyhow::Result<PermissionArtifacts> {
            Ok(PermissionArtifacts::default())
        }
        fn progress_fidelity(&self) -> ProgressFidelity {
            ProgressFidelity::Minimal
        }
        fn progress_observation_wiring(&self, _config: &ProgressObservationConfig) -> ProgressIngress {
            ProgressIngress::StdoutJsonl
        }
        fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
            boss_protocol::normalize_hook_event(raw)
        }
        fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
            match event {
                WorkerEvent::Stop {
                    session_id,
                    stop_hook_active,
                    stop_reason,
                } => Some(TurnEnd {
                    session_id: session_id.clone(),
                    reason: *stop_reason,
                    continuation: *stop_hook_active,
                }),
                _ => None,
            }
        }
        fn tool_use_interception_wiring(&self, _config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
            ToolUseInterceptionWiring::default()
        }
        fn agent_rules_preamble(&self) -> &'static str {
            ""
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
            WorkerErrorClass::Indeterminate
        }
        fn pane_monitor_spec(&self) -> Option<PaneMonitorSpec> {
            None
        }
        fn is_interrupt_recovery_turn_end(&self, raw: &serde_json::Value) -> bool {
            raw.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true)
        }
        fn structured_output_fallback(
            &self,
            _kind: boss_engine_structured_output::StructuredOutputKind,
            _text: &str,
        ) -> Vec<boss_engine_structured_output::fallback::FallbackCandidate> {
            Vec::new()
        }
    }

    /// Records every dispatched event for assertion. Cheaply `Clone` (an
    /// `Arc` around the shared log) so a test can hand one clone to
    /// [`run_interrupt_recovery`] (which takes its sink by value) while
    /// keeping another to inspect afterward.
    #[derive(Default, Clone)]
    struct RecordingSink {
        events: Arc<Mutex<Vec<IncomingHookEvent>>>,
    }

    #[async_trait]
    impl WorkerEventSink for RecordingSink {
        async fn dispatch_worker_event(&self, incoming: IncomingHookEvent) {
            self.events.lock().unwrap().push(incoming);
        }
    }

    fn snapshot(events_path: std::path::PathBuf, offset: u64, settle_window: Duration) -> InterruptRecoverySnapshot {
        InterruptRecoverySnapshot {
            events_path,
            offset,
            session_id: "sess-recovery-1".to_owned(),
            settle_window,
        }
    }

    #[tokio::test]
    async fn observes_a_cancellation_record_written_after_the_snapshot() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(&path, b"{\"type\":\"turn_started\"}\n").unwrap();
        let offset = std::fs::metadata(&path).unwrap().len();

        let driver: Arc<dyn AgentDriver> = Arc::new(FakeRecoveryDriver);
        let sink = RecordingSink::default();

        let write_path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            let mut existing = std::fs::read(&write_path).unwrap();
            existing.extend_from_slice(b"{\"type\":\"turn_ended\",\"cancelled\":true}\n");
            std::fs::write(&write_path, existing).unwrap();
        });

        let outcome = run_interrupt_recovery(
            driver,
            "run-obs-1".to_owned(),
            snapshot(path, offset, Duration::from_secs(5)),
            sink.clone(),
        )
        .await;

        assert_eq!(outcome, InterruptRecoveryOutcome::Observed);
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].is_turn_boundary());
    }

    #[tokio::test]
    async fn falls_back_to_a_synthesized_turn_end_when_nothing_appears() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        // No file at all yet — the fallback path must still work.

        let driver: Arc<dyn AgentDriver> = Arc::new(FakeRecoveryDriver);
        let sink = RecordingSink::default();

        let outcome = run_interrupt_recovery(
            driver,
            "run-fallback-1".to_owned(),
            snapshot(path, 0, Duration::from_millis(200)),
            sink.clone(),
        )
        .await;

        assert_eq!(outcome, InterruptRecoveryOutcome::Fallback);
        let events = sink.events.lock().unwrap();
        assert_eq!(
            events.len(),
            1,
            "the fallback must still unstick the slot with a Stop event"
        );
        assert!(events[0].is_turn_boundary());
    }

    #[tokio::test]
    async fn ignores_unrelated_lines_and_only_matches_cancellation() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(
            &path,
            b"{\"type\":\"phase_changed\"}\n{\"type\":\"turn_ended\",\"outcome\":\"completed\"}\n",
        )
        .unwrap();

        let driver: Arc<dyn AgentDriver> = Arc::new(FakeRecoveryDriver);
        let sink = RecordingSink::default();

        let outcome = run_interrupt_recovery(
            driver,
            "run-noise-1".to_owned(),
            snapshot(path, 0, Duration::from_millis(150)),
            sink.clone(),
        )
        .await;

        // Neither line is a cancellation, so this must fall back — proving
        // the matcher does not false-positive on unrelated event shapes.
        assert_eq!(outcome, InterruptRecoveryOutcome::Fallback);
        assert_eq!(sink.events.lock().unwrap().len(), 1);
    }
}
