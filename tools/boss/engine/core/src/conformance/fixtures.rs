//! Shared fixtures for the reference-driver conformance harness.
//!
//! Paired session transcripts captured against codex-cli
//! [`PINNED_CODEX_CLI_VERSION`] (see
//! `tools/boss/docs/investigations/codex-progress-channel-decision-2026-07-24.md`)
//! and rewritten so both transports carry the same logical session identity.
//! A Claude-hook transcript is the hook-ingress side; the Codex stdout JSONL
//! is the stdout-JSONL side. After normalisation both must yield the same
//! [`WorkerEvent`] sequence (ingress equivalence) and the same [`TurnEnd`]
//! (boundary equivalence).

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use boss_protocol::{NormalizeError, SessionStartSource, StopReason, WorkerEvent};

use crate::driver::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, EnvDirective, ModelMenu,
    PermissionArtifacts, PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig, SpawnPlan,
    SpawnRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass,
};
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{EffortLevel, ReasoningMode};

/// Codex CLI version the harness fixtures were captured against.
///
/// The Codex `--json` stream carries **no schema version field**. This pin is
/// the only defence against unannounced drift (removed flags, item-id base
/// changes, widened `error` meanings, new enum variants). Bump only after
/// re-running the investigation harness and updating fixtures.
pub const PINNED_CODEX_CLI_VERSION: &str = "0.145.0";

/// Item-id base observed on the pinned Codex CLI.
///
/// On 0.137.0 item ids were 1-based (`item_1`, `item_2`, …). On 0.145.0 the
/// investigation still saw `item_2`/`item_3` mid-turn (ids are opaque tokens
/// with a numeric suffix, not a dense 0-based counter starting at the first
/// envelope). The harness pins the *prefix form* `item_<n>` and the concrete
/// ids present in the fixtures; a base change that renumbers those ids must
/// break the pin, not be absorbed.
pub const PINNED_CODEX_ITEM_ID_BASE: &str = "item_";

/// Canonical session id shared by both fixture sides so `WorkerEvent`
/// sequences compare equal under [`PartialEq`].
pub const CANONICAL_SESSION_ID: &str = "019f974c-3d59-7533-b320-3963123c809b";

/// Bash command exercised by the paired session fixtures.
pub const FIXTURE_BASH_COMMAND: &str = "echo hooktest-exec";

/// Bash tool output exercised by the paired session fixtures.
pub const FIXTURE_BASH_OUTPUT: &str = "hooktest-exec\n";

/// Claude-hook ingress for an equivalent one-tool-turn session.
///
/// Shape matches Claude Code's wire format (and Codex's Claude-compatible
/// hook payloads). `UserPromptSubmit` is included so the logical turn is
/// complete: Claude fires it on a real session, and Codex's `turn.started`
/// is its stdout analog.
pub const CLAUDE_HOOK_SESSION_JSONL: &str = concat!(
    r#"{"session_id":"019f974c-3d59-7533-b320-3963123c809b","hook_event_name":"SessionStart","model":"gpt-5.5","permission_mode":"bypassPermissions","source":"startup"}"#,
    "\n",
    r#"{"session_id":"019f974c-3d59-7533-b320-3963123c809b","hook_event_name":"UserPromptSubmit","prompt":""}"#,
    "\n",
    r#"{"session_id":"019f974c-3d59-7533-b320-3963123c809b","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"echo hooktest-exec"},"tool_use_id":"call_fixture"}"#,
    "\n",
    r#"{"session_id":"019f974c-3d59-7533-b320-3963123c809b","hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"echo hooktest-exec"},"tool_response":"hooktest-exec\n","tool_use_id":"call_fixture"}"#,
    "\n",
    r#"{"session_id":"019f974c-3d59-7533-b320-3963123c809b","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"hooktest-exec"}"#,
    "\n",
);

/// Codex stdout-JSONL ingress for the same logical session.
///
/// **Shape-aligned and identity-rewritten**, not wire-identical to a live
/// capture: envelope types and item shapes come from the 0.145.0 investigation,
/// with `thread_id` and command text rewritten to [`CANONICAL_SESSION_ID`] /
/// [`FIXTURE_BASH_COMMAND`] so the two fixture sides compare under
/// [`PartialEq`]. Only `thread.started` carries `thread_id` on the wire;
/// later envelopes inherit session identity from sticky state in
/// [`CodexShapedDriver`] (as a production normaliser must).
///
/// The `agent_message` item has no activity-machine analog and must be
/// skipped by the normaliser (tolerance for unmappable variants).
///
/// Usage carries `cache_write_input_tokens` — the field added between 0.137.0
/// and 0.145.0. The normaliser must tolerate it; the version-pin tests assert
/// it is still present so a silent removal breaks the harness.
pub const CODEX_STDOUT_SESSION_JSONL: &str = concat!(
    r#"{"type":"thread.started","thread_id":"019f974c-3d59-7533-b320-3963123c809b"}"#,
    "\n",
    r#"{"type":"turn.started"}"#,
    "\n",
    r#"{"type":"item.started","item":{"id":"item_2","type":"command_execution","command":"echo hooktest-exec","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
    "\n",
    r#"{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"echo hooktest-exec","aggregated_output":"hooktest-exec\n","exit_code":0,"status":"completed"}}"#,
    "\n",
    r#"{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"hooktest-exec"}}"#,
    "\n",
    r#"{"type":"turn.completed","usage":{"input_tokens":25699,"cached_input_tokens":14080,"cache_write_input_tokens":0,"output_tokens":37,"reasoning_output_tokens":0}}"#,
    "\n",
);

/// Expected activity-machine event sequence for both fixture sides.
pub fn expected_session_events() -> Vec<WorkerEvent> {
    vec![
        WorkerEvent::SessionStart {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            source: SessionStartSource::Startup,
        },
        WorkerEvent::UserPromptSubmit {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            prompt: String::new(),
        },
        WorkerEvent::PreToolUse {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({"command": FIXTURE_BASH_COMMAND}),
        },
        WorkerEvent::PostToolUse {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            tool_name: "Bash".to_owned(),
            tool_input: serde_json::json!({"command": FIXTURE_BASH_COMMAND}),
            tool_response: serde_json::json!(FIXTURE_BASH_OUTPUT),
        },
        WorkerEvent::Stop {
            session_id: CANONICAL_SESSION_ID.to_owned(),
            stop_hook_active: false,
            stop_reason: StopReason::Completed,
        },
    ]
}

/// Rewrite every event's session id so sequences from drivers that use
/// different identity field names still compare equal.
pub fn normalize_session_id(mut event: WorkerEvent, session_id: &str) -> WorkerEvent {
    match &mut event {
        WorkerEvent::SessionStart { session_id: s, .. }
        | WorkerEvent::UserPromptSubmit { session_id: s, .. }
        | WorkerEvent::PreToolUse { session_id: s, .. }
        | WorkerEvent::PostToolUse { session_id: s, .. }
        | WorkerEvent::Stop { session_id: s, .. }
        | WorkerEvent::Notification { session_id: s, .. }
        | WorkerEvent::SessionEnd { session_id: s, .. } => {
            *s = session_id.to_owned();
        }
    }
    event
}

// ─── Codex exec spawn contract ───────────────────────────────────────────────

/// Flags every Codex `exec` spawn plan must include.
///
/// Shared between the conformance test double and (when it lands) production
/// `CodexDriver::spawn_invocation`. Pin tests call
/// [`assert_codex_exec_spawn_contract`] so a future production path that
/// regresses these flags fails the same pin without rewriting the double.
pub const CODEX_EXEC_REQUIRED_FLAGS: &[&str] = &["--json", "--strict-config"];

/// Long-form flags that must never appear on a Codex `exec` spawn line.
///
/// `-a` / `--ask-for-approval` was removed from `codex exec` on 0.145.0 and
/// produces a hard argument error.
pub const CODEX_EXEC_FORBIDDEN_LONG_FLAGS: &[&str] = &["--ask-for-approval"];

/// Build the reference `codex exec …` command line that satisfies
/// [`CODEX_EXEC_REQUIRED_FLAGS`] and omits every forbidden token.
///
/// Production `CodexDriver` may add model / sandbox tokens around these
/// required flags, but must still pass [`assert_codex_exec_spawn_contract`].
pub fn codex_exec_reference_command() -> String {
    // Required flags come from the shared constants so the double cannot
    // drift from the contract the pin tests enforce.
    let mut cmd = String::from("codex exec");
    for flag in CODEX_EXEC_REQUIRED_FLAGS {
        cmd.push(' ');
        cmd.push_str(flag);
    }
    cmd.push_str(" --dangerously-bypass-approvals-and-sandbox \"$(cat .codex/initial-prompt.txt)\"\n");
    cmd
}

/// Assert a [`SpawnPlan`] satisfies the Codex exec spawn contract.
///
/// Intended for version-pin tests and, later, for production-driver tests that
/// feed a real `CodexDriver` plan into the same checker.
pub fn assert_codex_exec_spawn_contract(plan: &SpawnPlan) {
    let tokens: Vec<&str> = plan.command.split_whitespace().collect();
    for flag in CODEX_EXEC_REQUIRED_FLAGS {
        assert!(
            plan.command.contains(flag),
            "Codex exec spawn contract requires `{flag}`; got {}",
            plan.command,
        );
    }
    for flag in CODEX_EXEC_FORBIDDEN_LONG_FLAGS {
        assert!(
            !plan.command.contains(flag),
            "Codex exec spawn contract forbids `{flag}` (removed on pinned CLI); got {}",
            plan.command,
        );
    }
    // Short `-a` is the removed ask-for-approval alias; match as a whole token
    // so we do not false-positive on longer flags that happen to contain "a".
    assert!(
        !tokens.contains(&"-a"),
        "Codex exec spawn contract forbids bare `-a` (removed ask-for-approval alias); got {}",
        plan.command,
    );
}

// ─── Codex-shaped reference normaliser (test double for a future CodexDriver) ─

fn no_effort_value(_level: EffortLevel) -> Option<&'static str> {
    None
}
fn only_model(_level: EffortLevel) -> &'static str {
    "gpt-5.5"
}
fn only_model_for_reasoning(_mode: ReasoningMode) -> &'static str {
    "gpt-5.5"
}
fn no_addendum(_level: EffortLevel) -> Option<&'static str> {
    None
}
fn never_auto_permissions(_model: &str) -> bool {
    false
}

/// Maps `codex exec --json` envelopes onto [`WorkerEvent`]s with shapes that
/// match Claude's hook normaliser for an equivalent session.
///
/// Session identity is sticky: `thread.started` records `thread_id`, and later
/// envelopes that omit `thread_id` (the real Codex wire shape) inherit it.
/// Envelopes that need a session before any `thread.started` fail with
/// [`NormalizeError::MissingField`] rather than falling back to a hardcoded id.
///
/// Deliberately rejects unmappable variants (`agent_message`, unknown types)
/// so the tolerance tests exercise the skip path. Tool inputs are lifted into
/// Claude's `{"command":…}` object form so cross-transport sequences compare
/// equal under [`PartialEq`] — the activity machine only reads `tool_name`
/// for PreToolUse, but the harness asserts field-level parity too.
pub struct CodexShapedDriver {
    descriptor: DriverDescriptor,
    /// Last `thread_id` observed on `thread.started` (and any later envelope
    /// that carries one). Interior mutability because [`AgentDriver`] methods
    /// take `&self` and the trait is `Send + Sync`.
    last_thread_id: Mutex<Option<String>>,
}

impl CodexShapedDriver {
    pub fn new() -> Self {
        Self {
            descriptor: DriverDescriptor {
                name: "codex-shaped",
                label: "Codex-shaped conformance driver",
                binary: "codex",
                config_dir: ".codex",
                agent_rules_filename: "AGENTS.md",
                initial_prompt_filename: "initial-prompt.txt",
                model_menu: ModelMenu {
                    engine_default: "gpt-5.5",
                    effort_value_for_level: no_effort_value,
                    default_model_for_level: only_model,
                    model_for_reasoning: only_model_for_reasoning,
                    prompt_addendum_for_level: no_addendum,
                    model_requires_auto_permissions: never_auto_permissions,
                },
            },
            last_thread_id: Mutex::new(None),
        }
    }

    /// Resolve session identity for a mappable envelope.
    ///
    /// Preference order: `thread_id` on this envelope (and sticky-update), else
    /// the last seen sticky id. Fails if neither is known.
    fn resolve_session_id(&self, obj: &serde_json::Map<String, serde_json::Value>) -> Result<String, NormalizeError> {
        if let Some(id) = obj.get("thread_id").and_then(|v| v.as_str()) {
            let id = id.to_owned();
            *self.last_thread_id.lock().expect("codex-shaped last_thread_id lock") = Some(id.clone());
            return Ok(id);
        }
        self.last_thread_id
            .lock()
            .expect("codex-shaped last_thread_id lock")
            .clone()
            .ok_or(NormalizeError::MissingField("thread_id"))
    }
}

impl Default for CodexShapedDriver {
    fn default() -> Self {
        Self::new()
    }
}

pub fn codex_shaped_driver() -> CodexShapedDriver {
    CodexShapedDriver::new()
}

#[async_trait]
impl AgentDriver for CodexShapedDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &self.descriptor
    }

    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new([Capability::ProgressObservation, Capability::TurnBoundary])
    }

    fn spawn_invocation(&self, _: SpawnRequest<'_>) -> SpawnPlan {
        // Build from the shared spawn contract so the double and the pin tests
        // share one source of truth. A production CodexDriver must pass the
        // same [`assert_codex_exec_spawn_contract`] check.
        SpawnPlan {
            env: vec![EnvDirective::Set("CODEX_HOME".into(), "/tmp/boss-codex-home".into())],
            command: codex_exec_reference_command(),
        }
    }

    async fn provision_workspace(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<Option<DriverRuntimeState>> {
        // Conformance fixture creates no out-of-workspace state.
        Ok(None)
    }

    async fn teardown_workspace(
        &self,
        _: Option<&Path>,
        _: &str,
        _: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn write_permission_config(&self, _: &PermissionInput, _: &Path) -> anyhow::Result<PermissionArtifacts> {
        unimplemented!("conformance fixture — not exercised")
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, _: &ProgressObservationConfig) -> ProgressIngress {
        ProgressIngress::StdoutJsonl
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        let obj = raw
            .as_object()
            .ok_or_else(|| NormalizeError::Malformed("expected JSON object".into()))?;
        let kind = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or(NormalizeError::MissingField("type"))?;
        let item = obj.get("item");
        let item_type = item.and_then(|i| i.get("type")).and_then(|v| v.as_str());

        // Unmappable variants are rejected before session resolution so a
        // bare error/agent_message/unknown item does not require sticky state.
        let is_mappable = matches!(
            (kind, item_type),
            ("thread.started", _)
                | ("turn.started", _)
                | ("item.started", Some("command_execution"))
                | ("item.completed", Some("command_execution"))
                | ("turn.completed", _)
        );
        if !is_mappable {
            return Err(NormalizeError::UnknownEvent(kind.to_owned()));
        }

        let session_id = if kind == "thread.started" {
            // thread.started must carry thread_id on the wire — do not inherit.
            let id = obj
                .get("thread_id")
                .and_then(|v| v.as_str())
                .ok_or(NormalizeError::MissingField("thread_id"))?
                .to_owned();
            *self.last_thread_id.lock().expect("codex-shaped last_thread_id lock") = Some(id.clone());
            id
        } else {
            self.resolve_session_id(obj)?
        };

        Ok(match (kind, item_type) {
            ("thread.started", _) => WorkerEvent::SessionStart {
                session_id,
                source: SessionStartSource::Startup,
            },
            ("turn.started", _) => WorkerEvent::UserPromptSubmit {
                session_id,
                prompt: String::new(),
            },
            ("item.started", Some("command_execution")) => {
                let command = item
                    .and_then(|i| i.get("command"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                WorkerEvent::PreToolUse {
                    session_id,
                    tool_name: "Bash".to_owned(),
                    // Lift a bare string command into Claude's object shape so
                    // the two transports produce PartialEq-equal events.
                    tool_input: match command {
                        serde_json::Value::String(s) => serde_json::json!({"command": s}),
                        other => other,
                    },
                }
            }
            ("item.completed", Some("command_execution")) => {
                let command = item
                    .and_then(|i| i.get("command"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let output = item
                    .and_then(|i| i.get("aggregated_output"))
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                WorkerEvent::PostToolUse {
                    session_id,
                    tool_name: "Bash".to_owned(),
                    tool_input: match command {
                        serde_json::Value::String(s) => serde_json::json!({"command": s}),
                        other => other,
                    },
                    tool_response: output,
                }
            }
            ("turn.completed", _) => WorkerEvent::Stop {
                session_id,
                stop_hook_active: false,
                stop_reason: StopReason::Completed,
            },
            // is_mappable already filtered unknowns; keep the arm exhaustive.
            (other, _) => return Err(NormalizeError::UnknownEvent(other.to_owned())),
        })
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        match event {
            WorkerEvent::Stop {
                session_id,
                stop_reason,
                ..
            } => Some(TurnEnd {
                session_id: session_id.clone(),
                reason: *stop_reason,
                // Codex has no stop-hook-active / continuation concept.
                continuation: false,
            }),
            _ => None,
        }
    }

    fn tool_use_interception_wiring(&self, _: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }

    fn agent_rules_preamble(&self) -> &'static str {
        ""
    }

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        let path = raw.get("transcript_path")?.as_str()?;
        if path.is_empty() { None } else { Some(path.to_owned()) }
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        raw
    }

    fn extract_error_from_transcript(&self, _: &[serde_json::Value]) -> Option<String> {
        None
    }

    fn classify_error(&self, _: &str) -> WorkerErrorClass {
        WorkerErrorClass::Indeterminate
    }

    fn structured_output_fallback(&self, _: StructuredOutputKind, _: &str) -> Vec<FallbackCandidate> {
        Vec::new()
    }
}

/// Decode every JSONL line through `driver`.
///
/// - [`NormalizeError::UnknownEvent`] is skipped (tolerance for unmappable
///   variants such as `agent_message` / unknown TurnItem types).
/// - [`NormalizeError::Malformed`] / [`NormalizeError::MissingField`] panic:
///   fixture streams and well-formed sessions must not produce them.
/// - Non-JSON lines also panic — fixtures are authored, not scraped live.
pub fn decode_jsonl(driver: &dyn AgentDriver, jsonl: &str) -> Vec<WorkerEvent> {
    let mut events = Vec::new();
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let raw: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("fixture JSONL line is not valid JSON ({e}): {line}"));
        match driver.normalize_progress_event(&raw) {
            Ok(event) => events.push(event),
            Err(NormalizeError::UnknownEvent(_)) => {
                // Skip path: unmappable / additive variants.
            }
            Err(err) => {
                panic!("unexpected normalize error for line {line}: {err}");
            }
        }
    }
    events
}

#[cfg(test)]
mod sticky_session_tests {
    use super::*;

    fn event_session_id(event: &WorkerEvent) -> &str {
        match event {
            WorkerEvent::SessionStart { session_id, .. }
            | WorkerEvent::UserPromptSubmit { session_id, .. }
            | WorkerEvent::PreToolUse { session_id, .. }
            | WorkerEvent::PostToolUse { session_id, .. }
            | WorkerEvent::Stop { session_id, .. }
            | WorkerEvent::Notification { session_id, .. }
            | WorkerEvent::SessionEnd { session_id, .. } => session_id,
        }
    }

    #[test]
    fn later_envelopes_inherit_thread_id_from_thread_started() {
        // Rewrite only thread.started's id. Later fixture lines already omit
        // thread_id (the real Codex wire shape); they must inherit the sticky
        // id rather than a hardcoded CANONICAL_SESSION_ID fallback.
        let custom_id = "session-from-thread-started-only";
        assert!(
            CODEX_STDOUT_SESSION_JSONL.contains(CANONICAL_SESSION_ID),
            "fixture must embed the canonical id on thread.started so rewrite is meaningful",
        );
        // Only one occurrence of the canonical id (on thread.started).
        assert_eq!(
            CODEX_STDOUT_SESSION_JSONL.matches(CANONICAL_SESSION_ID).count(),
            1,
            "fixture must carry thread_id only on thread.started",
        );
        let stream = CODEX_STDOUT_SESSION_JSONL.replace(CANONICAL_SESSION_ID, custom_id);
        let driver = codex_shaped_driver();
        let events = decode_jsonl(&driver, &stream);
        assert_eq!(
            events.len(),
            expected_session_events().len(),
            "full session must still decode after id rewrite",
        );
        for event in &events {
            assert_eq!(
                event_session_id(event),
                custom_id,
                "every event must inherit the rewritten thread.started id; got {event:?}",
            );
        }
    }

    #[test]
    fn mappable_envelope_without_known_session_fails() {
        let driver = codex_shaped_driver();
        let raw: serde_json::Value = serde_json::from_str(r#"{"type":"turn.started"}"#).unwrap();
        let err = driver
            .normalize_progress_event(&raw)
            .expect_err("turn.started with no prior thread.started must fail");
        assert!(matches!(err, NormalizeError::MissingField("thread_id")), "got {err:?}",);
    }

    #[test]
    fn thread_started_without_thread_id_fails() {
        let driver = codex_shaped_driver();
        let raw: serde_json::Value = serde_json::from_str(r#"{"type":"thread.started"}"#).unwrap();
        let err = driver
            .normalize_progress_event(&raw)
            .expect_err("thread.started must carry thread_id");
        assert!(matches!(err, NormalizeError::MissingField("thread_id")), "got {err:?}",);
    }
}
