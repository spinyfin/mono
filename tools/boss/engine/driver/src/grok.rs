//! `GrokDriver` — xAI Grok Build agent driver (skeleton).
//!
//! Descriptor, capability set, model menu, and registry entry only.
//! Spawning, workspace provisioning, hook wiring, progress observation,
//! control verbs, and transcript access are follow-on work — see
//! `tools/boss/docs/designs/grok-as-a-first-class-interactive-agent-driver.md`
//! (T-07 onward). This module deliberately contains **no** spawn line,
//! **no** hook wiring, and **no** runtime behaviour.
//!
//! Behavioural fan-out lives under the `grok/` submodule directory so
//! concurrent follow-on tasks (transcript, control verbs, output capture,
//! characterisation) do not serialise on this file.

use std::path::Path;

use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{NormalizeError, WorkerEvent};
use serde_json::{Value, json};

mod model_menu;

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, ModelMenu, PermissionArtifacts,
    PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig, SpawnPlan, SpawnRequest,
    ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass,
};

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------
//
// Model menu refresh path: `grok models` is the machine-readable source.
// The one-model table below (`grok-4.5` only) will be wrong the moment xAI
// ships a second SKU — update engine_default / model_for_reasoning /
// default_model_for_level together and add a live-catalog conformance
// assertion (design A-11 / T-20). Do not hard-freeze forever, and never
// reintroduce `grok-build-0.1` (not on the account menu) or
// `grok-code-fast-1` (retired; silently redirects).

static GROK_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "grok",
    label: "Grok Build",
    binary: "grok",
    config_dir: ".grok",
    // Grok natively resolves workspace-root `AGENTS.md` as project
    // instructions (design §Config discovery). Same filename Codex uses.
    agent_rules_filename: "AGENTS.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        // Sole model on `grok models` (0.2.112, 2026-07-27). Step-5
        // fall-through and every classified row both resolve here until a
        // second SKU appears.
        engine_default: "grok-4.5",
        effort_value_for_level: model_menu::effort_value_for_level,
        default_model_for_level: model_menu::default_model_for_level,
        model_for_reasoning: model_menu::model_for_reasoning,
        prompt_addendum_for_level: model_menu::prompt_addendum_for_level,
        model_requires_auto_permissions: model_menu::model_requires_auto_permissions,
    },
};

/// Preamble for the agent-rules file (`AGENTS.md`). Names Grok observability
/// rather than Claude hooks so the shared body below it is not lying about
/// the mechanism this session uses. Mechanism prose is filled in when
/// provision/hooks land; the skeleton only needs a distinctive, accurate
/// session framing.
const GROK_AGENT_RULES_PREAMBLE: &str = "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via Grok hooks under a Boss-owned GROK_HOME.\n\
     For ordinary pre-push validation, run `checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.";

// ---------------------------------------------------------------------------
// GrokDriver
// ---------------------------------------------------------------------------

/// xAI Grok Build CLI driver.
///
/// Registered under the `"grok"` slug. Declares the v1 capability set from
/// the Grok driver design; behavioural methods are `unimplemented!()` (or
/// honest no-ops on hot paths) until the corresponding follow-on tasks
/// land. No spawning, no wiring, no hooks in this skeleton.
#[derive(Default)]
pub struct GrokDriver {
    // Keep this type non-unit so callers can use `Default` uniformly with
    // stateful drivers without tripping clippy's unit-default lint.
    _private: (),
}

#[async_trait]
impl AgentDriver for GrokDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &GROK_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Capability declaration for GrokDriver (v1) — design doc
        // §Capability declaration for GrokDriver (v1). Every omission
        // below is deliberate; each notes its absence disposition and why.
        //
        // Provided (all except ToolProvisioning + AwaitingInputSignal):
        //   Spawn, WorkspaceProvisioning, PermissionPolicy, ModelAndEffortMenu,
        //   ProgressObservation, ToolUseInterception (deny-only), TurnBoundary,
        //   StructuredOutput, TranscriptAccess, ControlVerbs, PromptComposition.
        //
        // ToolUseInterception is **deny-only**: Grok PreToolUse accepts deny
        // but `updatedInput` rewrite did not apply (spike + design G-6). The
        // trait rewrite path is unreachable; inline-`--body` editorial cases
        // become Deny-with-reason — same call as Codex.
        //
        // Two honesty gates attach to the declarations rather than
        // qualifying them (hooks adapter / compat-permission leak); those
        // are follow-on work. The set itself is settled.
        CapabilitySet::new([
            Capability::Spawn,
            Capability::WorkspaceProvisioning,
            Capability::PermissionPolicy,
            Capability::ModelAndEffortMenu,
            Capability::ProgressObservation,
            Capability::ToolUseInterception,
            Capability::TurnBoundary,
            Capability::StructuredOutput,
            Capability::TranscriptAccess,
            Capability::ControlVerbs,
            Capability::PromptComposition,
            // ToolProvisioning — omitted → default Degrade. Unused in v1 for
            // every driver (including Claude, which *declares* it but injects
            // nothing). Grok has MCP/plugins/skills/subagents but Boss
            // injects none; the driver will explicitly disable subagents and
            // memory at spawn. Declaring it would overclaim. Degrade is
            // correct: no dispatch refusal, no synthesised tooling.
            //
            // AwaitingInputSignal — omitted → default Degrade (never
            // Synthesize). Grok fires `Notification` with `notificationType`
            // / `level`, but the vocabulary is uncharacterised and the
            // capability's contract forbids guessing this state from a
            // lower-fidelity channel. A Grok worker shows Working/Idle and
            // never a fabricated WaitingForInput (design G-13 / T-24).
        ])
    }

    fn spawn_invocation(&self, _request: SpawnRequest<'_>) -> SpawnPlan {
        // Follow-on: T-08. Skeleton must not emit a launch line.
        unimplemented!("GrokDriver::spawn_invocation — not yet implemented (skeleton)")
    }

    async fn provision_workspace(
        &self,
        _workspace: &Path,
        _prompt_text: &str,
        _run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>> {
        // Follow-on: T-07.
        unimplemented!("GrokDriver::provision_workspace — not yet implemented (skeleton)")
    }

    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        _runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        // Follow-on: paired with provision. No-op would be honest only after
        // provision exists and returns None; leave unimplemented until then.
        unimplemented!("GrokDriver::teardown_workspace — not yet implemented (skeleton)")
    }

    async fn write_permission_config(
        &self,
        _input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        // Follow-on: T-17 (gated on T-01 compat-leak investigation).
        unimplemented!("GrokDriver::write_permission_config — not yet implemented (skeleton)")
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // Design G-5: Rich — per-tool PreToolUse/PostToolUse events, same
        // tier as Claude. Declared even before wiring lands so the
        // stale-worker sweep threshold is correct once a Grok worker runs.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, _config: &ProgressObservationConfig) -> ProgressIngress {
        // Follow-on: T-09. Skeleton must not emit hook wiring that would be
        // merged into a Claude settings.json the Grok process never reads.
        unimplemented!("GrokDriver::progress_observation_wiring — not yet implemented (skeleton)")
    }

    fn normalize_progress_event(&self, _raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        // Follow-on: T-10 (Grok camelCase / snake_case event dialect).
        unimplemented!("GrokDriver::normalize_progress_event — not yet implemented (skeleton)")
    }

    fn turn_boundary(&self, _event: &WorkerEvent) -> Option<TurnEnd> {
        // Honest "no boundary yet" for a skeleton that has no normaliser.
        // Declaring TurnBoundary in the set is the promise; the method
        // implementation lands with progress normalise (T-10 / T-12).
        None
    }

    fn tool_use_interception_wiring(&self, _config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        // Follow-on: T-09. Must not return Claude guard scripts against a
        // Grok payload dialect (they would fail open — design §Guardrail
        // integrity). Empty wiring until the canonicalisation adapter lands.
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }

    fn agent_rules_preamble(&self) -> &'static str {
        GROK_AGENT_RULES_PREAMBLE
    }

    fn transcript_path_for_session(&self, _raw: &serde_json::Value) -> Option<String> {
        // Follow-on: Grok stamps `transcriptPath` on every hook payload.
        None
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        // Minimal ACP updates.jsonl → canonical reshape so Stop-boundary
        // marker scans can read Grok transcripts once `transcript_path`
        // points at `$GROK_HOME/sessions/…/updates.jsonl`. Full multi-chunk
        // accumulation and tool_call correlation remain follow-on work
        // (TranscriptSessionNormalizer); a single final `agent_message_chunk`
        // is the common shape for short `[blocked]` answers and is enough
        // for the conformance fixture.
        normalize_acp_updates_entry(raw)
    }

    fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
        None
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        // Must not route through `classify_claude_error`. Real Grok/xAI
        // classification is ControlVerbs follow-on (T-13). Indeterminate is
        // the documented "recognised as an error but not confidently
        // bucketed; treat as Permanent" class.
        WorkerErrorClass::Indeterminate
    }

    // mid_turn_pane_input: intentionally NOT overridden. Trait default is
    // Rejects — the safe answer until mid-turn stdin consumption is proven
    // empirically for the interactive TUI (design G-10 / T-13). Structural
    // arguments that Grok "should" Buffer are not enough to flip the
    // default.

    fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
        // No Grok-specific prose-scrape conventions yet. Empty Vec: primary
        // channel is the file contract (default structured_output_wiring).
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// ACP updates.jsonl → canonical entry reshape
// ---------------------------------------------------------------------------
//
// Spike samples (ghostty-grok-pane-viability-artifacts) write records like:
//   {"method":"session/update","params":{"update":{
//     "sessionUpdate":"agent_message_chunk",
//     "content":{"type":"text","text":"…"}
//   }}}
// Unrecognised / non-prose records pass through unchanged so the Claude-family
// values parser skips them; this must never panic on the live-status path.

fn normalize_acp_updates_entry(raw: Value) -> Value {
    let Some(update) = raw
        .get("params")
        .and_then(|params| params.get("update"))
        .and_then(|update| update.as_object())
    else {
        return raw;
    };
    let Some(kind) = update.get("sessionUpdate").and_then(Value::as_str) else {
        return raw;
    };
    match kind {
        "agent_message_chunk" => {
            let Some(text) = update
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
            else {
                return raw;
            };
            json!({
                "type": "assistant",
                "content": [{"type": "text", "text": text}],
            })
        }
        "user_message_chunk" => {
            let Some(text) = update
                .get("content")
                .and_then(|content| content.get("text"))
                .and_then(Value::as_str)
            else {
                return raw;
            };
            json!({
                "type": "user",
                "text": text,
            })
        }
        // Non-prose ACP records (tool_call, turn_completed, hook_execution, …)
        // stay opaque until the full session normalizer lands.
        _ => raw,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbsenceDisposition, Capability, MidTurnPaneInput};
    use boss_protocol::{EffortLevel, ReasoningMode};

    #[test]
    fn normalize_transcript_entry_surfaces_agent_message_chunk() {
        let raw = json!({
            "timestamp": 1,
            "method": "session/update",
            "params": {
                "sessionId": "s1",
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "[blocked] reason=\"need a decision\""}
                }
            }
        });
        assert_eq!(
            GrokDriver::default().normalize_transcript_entry(raw),
            json!({
                "type": "assistant",
                "content": [{"type": "text", "text": "[blocked] reason=\"need a decision\""}],
            }),
        );
    }

    #[test]
    fn normalize_transcript_entry_passes_through_non_acp_records() {
        let raw = json!({"type": "assistant", "content": [{"type": "text", "text": "already canonical"}]});
        assert_eq!(GrokDriver::default().normalize_transcript_entry(raw.clone()), raw);
    }

    #[test]
    fn grok_descriptor_matches_design() {
        let driver = GrokDriver::default();
        let d = driver.descriptor();
        assert_eq!(d.name, "grok");
        assert_eq!(d.label, "Grok Build");
        assert_eq!(d.binary, "grok");
        assert_eq!(d.config_dir, ".grok");
        assert_eq!(d.agent_rules_filename, "AGENTS.md");
        assert_eq!(d.initial_prompt_filename, "initial-prompt.txt");
        assert_eq!(d.model_menu.engine_default, "grok-4.5");
    }

    #[test]
    fn grok_model_menu_is_single_sku_five_of_seven_effort() {
        let driver = GrokDriver::default();
        let menu = &driver.descriptor().model_menu;
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Trivial), Some("low"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Small), Some("medium"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Medium), Some("high"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Large), Some("xhigh"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Max), Some("max"));
        // Both reasoning modes share the sole SKU.
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Standard), "grok-4.5");
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Investigation), "grok-4.5");
        assert_eq!((menu.default_model_for_level)(EffortLevel::Trivial), "grok-4.5");
        assert_eq!((menu.default_model_for_level)(EffortLevel::Max), "grok-4.5");
        assert!(!(menu.model_requires_auto_permissions)("grok-4.5"));
    }

    #[test]
    fn grok_declares_design_capability_set() {
        let caps = GrokDriver::default().capabilities();
        for cap in [
            Capability::Spawn,
            Capability::WorkspaceProvisioning,
            Capability::PermissionPolicy,
            Capability::ModelAndEffortMenu,
            Capability::ProgressObservation,
            Capability::ToolUseInterception,
            Capability::TurnBoundary,
            Capability::StructuredOutput,
            Capability::TranscriptAccess,
            Capability::ControlVerbs,
            Capability::PromptComposition,
        ] {
            assert!(caps.provides(cap), "GrokDriver must provide {cap:?}");
        }
        assert!(!caps.provides(Capability::ToolProvisioning));
        assert!(!caps.provides(Capability::AwaitingInputSignal));
        assert_eq!(
            caps.absence_disposition(Capability::ToolProvisioning),
            AbsenceDisposition::Degrade
        );
        assert_eq!(
            caps.absence_disposition(Capability::AwaitingInputSignal),
            AbsenceDisposition::Degrade
        );
        // AwaitingInputSignal must never be Synthesize — capability contract
        // and design G-13 both forbid fabricating WaitingForInput.
        assert_ne!(
            caps.absence_disposition(Capability::AwaitingInputSignal),
            AbsenceDisposition::Synthesize
        );
    }

    #[test]
    fn grok_mid_turn_pane_input_stays_at_safe_rejects_default() {
        // Must not claim Buffers without empirical mid-turn evidence
        // (design G-10). Trait default is Rejects; do not override yet.
        let driver = GrokDriver::default();
        assert_eq!(driver.mid_turn_pane_input(), MidTurnPaneInput::Rejects);
        assert!(!driver.mid_turn_pane_input().buffers());
    }

    #[test]
    fn grok_progress_fidelity_is_rich() {
        assert_eq!(GrokDriver::default().progress_fidelity(), ProgressFidelity::Rich);
    }
}
