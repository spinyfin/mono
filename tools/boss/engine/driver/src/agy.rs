//! `AgyDriver` — Google Antigravity (`agy`) agent driver skeleton.
//!
//! Descriptor, capability set, and Gemini-family model menu for Boss's
//! fourth driver. Pane spawn, per-run `HOME` provisioning, hook wiring,
//! and Control Verbs beyond the interrupt declaration land in follow-on
//! tasks — this module is the seed that keeps the registry and
//! conformance suite green once the slug is registered.
//!
//! Qualified against `agy` 1.5.7 or later. `auto_approve` hook verdicts
//! were not measured on 1.5.5 (permission / shared-HOME studies,
//! 2026-11-14).

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{NormalizeError, WorkerEvent};
use boss_ssh_transport::shell_quote;

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, HookWiringDestination,
    InterruptDelivery, InterruptGesture, InterruptPlan, MidTurnPaneInput, ModelMenu, PermissionArtifacts,
    PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig, ProgressObservationWiring,
    SpawnPlan, SpawnRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd, TurnEndEvidence,
    WorkerErrorClass,
};

mod model_menu;
mod transcript;

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------
//
// Model menu refresh path: `agy models` is the machine-readable source.
// Update the family constants in [`model_menu`] together with
// engine_default / model_for_reasoning / default_model_for_level /
// review_model_for_tier / design_investigation_model. Do not dispatch
// Claude, GPT-OSS, or orchestrator ids through this driver.

static AGY_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "agy",
    label: "Antigravity",
    binary: "agy",
    // Boss-owned control directory for the initial-prompt launcher, matching
    // Grok's `.grok`. `agy` itself has no config-dir flag; per-run state
    // lives under a scoped HOME (later provisioning task).
    config_dir: ".agy",
    agent_rules_filename: "AGENTS.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        engine_default: model_menu::FAMILY_HYDROGEN,
        effort_value_for_level: model_menu::effort_value_for_level,
        default_model_for_level: model_menu::default_model_for_level,
        model_for_reasoning: model_menu::model_for_reasoning,
        review_model_for_tier: model_menu::review_model_for_tier,
        design_investigation_model: Some(model_menu::design_investigation_model),
        prompt_addendum_for_level: model_menu::prompt_addendum_for_level,
        model_requires_auto_permissions: model_menu::model_requires_auto_permissions,
        model_belongs_to_driver: model_menu::model_belongs_to_driver,
    },
};

const AGY_AGENT_RULES_PREAMBLE: &str = "You are running inside a managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via Antigravity hooks under an engine-owned per-run HOME.\n\
     For ordinary pre-push validation, run `checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.\n\
     Do not wrap `bazel build`, `bazel test`, `bazel run`, or `checkleft run`\n\
     in a shell `timeout`.";

/// Google Antigravity CLI driver.
///
/// Registered under the `"agy"` slug. Declares the v1 capability set from
/// the Antigravity driver design. Spawn, provisioning, and hook wiring are
/// stubs in this seed; later tasks replace them.
#[derive(Default)]
pub struct AgyDriver {
    _private: (),
}

/// Build the skeleton pane command: `agy --model <composed catalogue id>`.
///
/// Effort is encoded in the model-id suffix (`-low`/`-medium`/`-high`/
/// `-xhigh`) and clamped to the family's catalogue ceiling. `--effort` is
/// omitted so it cannot contradict that suffix. HOME wrapping, the
/// initial-brief launcher, and spawn flags land in the pane-spawn task.
pub fn build_agy_pane_command(request: &SpawnRequest<'_>) -> String {
    let model = model_menu::compose_model_id(request.model, request.effort);
    format!("agy --model {}\n", shell_quote(&model))
}

#[async_trait]
impl AgentDriver for AgyDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &AGY_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Capability declaration for AgyDriver (v1) — design doc
        // §Capability declaration. Every omission below is deliberate;
        // each notes its absence disposition and the phase-0 evidence.
        //
        // Provided:
        //   Spawn, WorkspaceProvisioning, PermissionPolicy, ModelAndEffortMenu,
        //   ProgressObservation, ToolUseInterception (rewrite-or-deny),
        //   TurnBoundary, StructuredOutput, TranscriptAccess, ControlVerbs,
        //   PromptComposition.
        //
        // ToolUseInterception is rewrite-or-deny: PreToolUse rewrites
        // arguments through `overwrite` on both measured binaries (hook
        // surface study, "PreToolUse can rewrite arguments"). The adapter
        // that actually emits `overwrite` is a later task; the declaration
        // matches the measured surface so `--driver agy` can clear the
        // design-family gate once an explicit pin is set.
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
            // every driver. `agy` has subagents (`invoke_subagent`) and no
            // disable flag; Boss injects none. Declaring it would overclaim.
            //
            // AwaitingInputSignal — omitted → default Degrade (never
            // Synthesize). `agy`'s hook set has no Notification-shaped
            // event (design §What agy is; pane study). Nothing honest to
            // bind to: an agy worker shows Working/Idle and never a
            // fabricated WaitingForInput.
            //
            // CommandOutcomeObservation — omitted → default Degrade (never
            // Synthesize). Measured absent: across 36 PostToolUse payloads
            // the union of fields contains `error` but no structured exit
            // status; an intentional `exit 7` produced PostToolUse.error=""
            // (hook surface study, "PostToolUse does not establish command
            // outcome"). This also keeps agy out of ConflictResolution /
            // CiRemediation by way of the gate.
        ])
    }

    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan {
        SpawnPlan {
            env: Vec::new(),
            command: build_agy_pane_command(&request),
        }
    }

    async fn provision_workspace(
        &self,
        _workspace: &Path,
        _prompt_text: &str,
        _run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>> {
        Ok(None)
    }

    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        _runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn write_permission_config(
        &self,
        _input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        Ok(PermissionArtifacts::default())
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // PreToolUse / PostToolUse fire in the interactive TUI (pane
        // viability study Q2). Declared Rich so the stale-worker sweep
        // threshold is correct once hook wiring lands.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, _config: &ProgressObservationConfig) -> ProgressIngress {
        // Destination is DriverOwned: hooks are discovered from
        // `$HOME/.gemini/config/hooks.json`, never from the Claude worker
        // settings file. The map is empty in this seed; the hook-wiring
        // task fills it. Must not return StdoutJsonl — the registry
        // refuses that topology for built-in drivers.
        ProgressIngress::HookCallback(ProgressObservationWiring {
            hooks: serde_json::Map::new(),
            destination: HookWiringDestination::DriverOwned,
        })
    }

    fn normalize_progress_event(&self, _raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        Err(NormalizeError::Malformed(
            "agy progress observation is not wired in this skeleton".into(),
        ))
    }

    fn turn_boundary(&self, _event: &WorkerEvent) -> Option<TurnEnd> {
        None
    }

    fn tool_use_interception_wiring(&self, _config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }

    fn agent_rules_preamble(&self) -> &'static str {
        AGY_AGENT_RULES_PREAMBLE
    }

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        // `agy` stamps `transcriptPath` on the Stop payload (pane study).
        // Empty strings are treated as missing, matching Claude/Grok.
        let s = raw
            .get("transcriptPath")
            .or_else(|| raw.get("transcript_path"))
            .and_then(|v| v.as_str())?;
        if s.is_empty() { None } else { Some(s.to_owned()) }
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        transcript::normalize_transcript_entry(raw)
    }

    fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
        None
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        WorkerErrorClass::Indeterminate
    }

    fn probe(&self) -> super::ProbeDelivery {
        super::ProbeDelivery::PaneText
    }

    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::PaneEsc
    }

    /// One Escape, confirmed through the interrupt-recovery observer.
    ///
    /// Measured in `docs/investigations/ghostty-agy-pane-viability.md` Q6:
    /// Esc cancels the in-flight turn and the process survives, but the
    /// cancelled turn **skips the `Stop` hook**. The only records are a
    /// `--log-file` line `Cancelling in-progress response for conversation
    /// <uuid>` and pane text `Interrupted · What should Antigravity CLI do
    /// instead?`. `transcript_full.jsonl` does not record the cancellation.
    /// Full recovery/stop/probe wiring belongs to a later Control Verbs
    /// task; this plan is what keeps the registry interrupt-declaration
    /// guard and the conformance interrupt-plan suite green.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        Some(InterruptPlan {
            gesture: InterruptGesture {
                key: "Escape",
                presses: 1,
                press_interval: Duration::from_millis(120),
            },
            confirm_window: Duration::from_secs(8),
            max_attempts: 2,
            turn_end_evidence: TurnEndEvidence::RecoveryObserver,
        })
    }

    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        // Pane viability study Q5: mid-turn stdin is buffered as the next
        // prompt. Override the safe Rejects default with that measurement.
        MidTurnPaneInput::Buffers
    }

    fn is_interrupt_recovery_turn_end(&self, raw: &serde_json::Value) -> bool {
        // Skeleton matcher for the measured Esc-cancel log line. A later
        // Control Verbs task replaces this with the real recovery observer
        // that tails `--log-file`.
        raw.get("type").and_then(|v| v.as_str()) == Some("log")
            && raw
                .get("message")
                .and_then(|v| v.as_str())
                .is_some_and(|message| message.contains("Cancelling in-progress response for conversation"))
    }

    fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AbsenceDisposition;
    use boss_protocol::{EffortLevel, ReasoningMode, ReviewModelTier};

    fn spawn_request<'a>(model: &'a str, effort: Option<&'a str>) -> SpawnRequest<'a> {
        SpawnRequest {
            model,
            effort,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: None,
        }
    }

    #[test]
    fn descriptor_matches_the_seed_contract() {
        let driver = AgyDriver::default();
        let d = driver.descriptor();
        assert_eq!(d.name, "agy");
        assert_eq!(d.label, "Antigravity");
        assert_eq!(d.binary, "agy");
        assert_eq!(d.config_dir, ".agy");
        assert_eq!(d.agent_rules_filename, "AGENTS.md");
        assert_eq!(d.initial_prompt_filename, "initial-prompt.txt");
        assert_eq!(d.model_menu.engine_default, model_menu::FAMILY_HYDROGEN);
        assert_eq!(
            (d.model_menu.design_investigation_model.expect("agy names Carbon"))(),
            model_menu::FAMILY_CARBON,
        );
    }

    #[test]
    fn model_menu_maps_every_reasoning_and_effort_pair() {
        let driver = AgyDriver::default();
        let menu = &driver.descriptor().model_menu;
        let levels = [
            EffortLevel::Trivial,
            EffortLevel::Small,
            EffortLevel::Medium,
            EffortLevel::Large,
            EffortLevel::Max,
        ];
        for reasoning in [ReasoningMode::Standard, ReasoningMode::Investigation] {
            let family = (menu.model_for_reasoning)(reasoning);
            let expected_family = match reasoning {
                ReasoningMode::Standard => model_menu::FAMILY_HYDROGEN,
                ReasoningMode::Investigation => model_menu::FAMILY_CARBON,
            };
            assert_eq!(family, expected_family, "{reasoning:?}");
            for level in levels {
                let effort = (menu.effort_value_for_level)(level);
                let composed = model_menu::compose_model_id(family, effort);
                let expected = match (reasoning, level) {
                    (ReasoningMode::Standard, EffortLevel::Trivial) => "gemini-4-hydrogen-low",
                    (ReasoningMode::Standard, EffortLevel::Small) => "gemini-4-hydrogen-medium",
                    (ReasoningMode::Standard, EffortLevel::Medium) => "gemini-4-hydrogen-high",
                    (ReasoningMode::Standard, EffortLevel::Large | EffortLevel::Max) => "gemini-4-hydrogen-xhigh",
                    (ReasoningMode::Investigation, EffortLevel::Trivial) => "gemini-4-carbon-low",
                    (ReasoningMode::Investigation, EffortLevel::Small) => "gemini-4-carbon-medium",
                    (ReasoningMode::Investigation, EffortLevel::Medium | EffortLevel::Large | EffortLevel::Max) => {
                        "gemini-4-carbon-high"
                    }
                };
                assert_eq!(composed, expected, "{reasoning:?} {level:?}");
                let command = AgyDriver::default()
                    .spawn_invocation(spawn_request(family, effort))
                    .command;
                assert!(
                    command.contains(&format!("--model '{composed}'")),
                    "{reasoning:?} {level:?} spawn must show {composed}, got {command}"
                );
                assert!(
                    !command.contains("--effort"),
                    "{reasoning:?} {level:?} must not emit --effort, got {command}"
                );
            }
        }
    }

    #[test]
    fn review_and_legacy_tables_follow_the_family_choices() {
        let driver = AgyDriver::default();
        let menu = &driver.descriptor().model_menu;
        assert_eq!(
            (menu.review_model_for_tier)(ReviewModelTier::Fast),
            model_menu::FAMILY_HYDROGEN
        );
        assert_eq!(
            (menu.review_model_for_tier)(ReviewModelTier::Balanced),
            model_menu::FAMILY_HYDROGEN
        );
        assert_eq!(
            (menu.review_model_for_tier)(ReviewModelTier::Strong),
            model_menu::FAMILY_CARBON
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Trivial),
            model_menu::FAMILY_FLASH
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Small),
            model_menu::FAMILY_HYDROGEN
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Medium),
            model_menu::FAMILY_HYDROGEN
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Large),
            model_menu::FAMILY_CARBON
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Max),
            model_menu::FAMILY_CARBON
        );
        assert!((menu.model_belongs_to_driver)("gemini-4-hydrogen-xhigh"));
        assert!((menu.model_belongs_to_driver)("GEMINI-4-CARBON-HIGH"));
        assert!((menu.model_belongs_to_driver)("gemini-3.8-flash-low"));
        assert!((menu.model_belongs_to_driver)("gemini-4-argon-medium"));
        assert!(!(menu.model_belongs_to_driver)("opus"));
        assert!(!(menu.model_belongs_to_driver)("claude-opus-5-5-high"));
        assert!(!(menu.model_belongs_to_driver)("gpt-oss-120b-medium"));
        assert!(!(menu.model_belongs_to_driver)("gemini-pro-orchestrator"));
        assert!(!(menu.model_requires_auto_permissions)(model_menu::FAMILY_HYDROGEN));
    }

    #[test]
    fn spawn_clamps_design_xhigh_on_carbon() {
        let command = AgyDriver::default()
            .spawn_invocation(spawn_request(model_menu::FAMILY_CARBON, Some("xhigh")))
            .command;
        assert!(
            command.contains("--model 'gemini-4-carbon-high'"),
            "Carbon has no xhigh id, got {command}"
        );
        assert!(
            !command.contains("xhigh"),
            "clamped Carbon must not keep xhigh: {command}"
        );
    }

    #[test]
    fn declares_v1_capability_set_with_measured_omissions() {
        let caps = AgyDriver::default().capabilities();
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
            assert!(caps.provides(cap), "AgyDriver must provide {cap:?}");
        }
        assert!(!caps.provides(Capability::ToolProvisioning));
        assert!(!caps.provides(Capability::AwaitingInputSignal));
        assert!(!caps.provides(Capability::CommandOutcomeObservation));
        assert_eq!(
            caps.absence_disposition(Capability::AwaitingInputSignal),
            AbsenceDisposition::Degrade
        );
        assert_ne!(
            caps.absence_disposition(Capability::AwaitingInputSignal),
            AbsenceDisposition::Synthesize
        );
        assert_eq!(
            caps.absence_disposition(Capability::CommandOutcomeObservation),
            AbsenceDisposition::Degrade
        );
        assert_ne!(
            caps.absence_disposition(Capability::CommandOutcomeObservation),
            AbsenceDisposition::Synthesize
        );
    }

    #[test]
    fn mid_turn_input_buffers_as_measured() {
        assert_eq!(AgyDriver::default().mid_turn_pane_input(), MidTurnPaneInput::Buffers);
    }

    #[test]
    fn interrupt_uses_esc_and_recovery_observer() {
        let driver = AgyDriver::default();
        assert_eq!(driver.interrupt(), InterruptDelivery::PaneEsc);
        let plan = driver.interrupt_plan().expect("agy declares an interrupt plan");
        assert_eq!(plan.gesture.key, "Escape");
        assert_eq!(plan.gesture.presses, 1);
        assert_eq!(plan.turn_end_evidence, TurnEndEvidence::RecoveryObserver);
        assert!(driver.is_interrupt_recovery_turn_end(&serde_json::json!({
            "type": "log",
            "message": "Cancelling in-progress response for conversation 9476903e-615e-4a03-b5b5-ea82733cc97b",
        })));
        assert!(!driver.is_interrupt_recovery_turn_end(&serde_json::json!({
            "type": "turn_ended",
            "outcome": "cancelled",
        })));
    }

    #[test]
    fn progress_wiring_is_driver_owned_hooks() {
        let ingress = AgyDriver::default().progress_observation_wiring(&ProgressObservationConfig {
            events_socket_path: std::path::PathBuf::from("/tmp/events.sock"),
            lease_id: "lease".into(),
            run_id: "run".into(),
            workspace_path: std::path::PathBuf::from("/tmp/ws"),
            forwarder_binary: std::path::PathBuf::from("/tmp/boss-event"),
        });
        match ingress {
            ProgressIngress::HookCallback(wiring) => {
                assert_eq!(wiring.destination, HookWiringDestination::DriverOwned);
                assert!(wiring.hooks.is_empty(), "skeleton has no hook map yet");
            }
            other => panic!("expected HookCallback(DriverOwned), got {other:?}"),
        }
        assert_eq!(AgyDriver::default().progress_fidelity(), ProgressFidelity::Rich);
    }
}
