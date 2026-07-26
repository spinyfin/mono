//! `CodexDriver` — skeleton of the OpenAI Codex agent driver.
//!
//! Declares the descriptor, capability set, and model menu for Codex. No
//! spawning or progress normalisation yet — those are follow-on design rows
//! (spawn/provisioning and the progress normaliser). Behavioural methods that
//! are trivial stay correct; methods that would otherwise silently no-op
//! return a clear `unimplemented!` so a call site cannot mistake absence of
//! work for success.
//!
//! See `tools/boss/docs/designs/codex-as-a-first-class-agent-driver.md`
//! (capability declaration for `CodexDriver` v1) and
//! `tools/boss/docs/investigations/ghostty-codex-pane-viability.md` Q1 for
//! the progress-ingress transport caveat.

use std::path::Path;

use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{EffortLevel, NormalizeError, ReasoningMode, WorkerEvent};

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, ModelMenu, PermissionArtifacts,
    PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig, SpawnPlan, SpawnRequest,
    StructuredOutputArtifacts, StructuredOutputRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd,
    WorkerErrorClass, default_structured_output_wiring,
};

// ---------------------------------------------------------------------------
// Codex model / effort menu
// ---------------------------------------------------------------------------
//
// Sourced from `codex debug models` on codex-cli 0.145.0 (2026-07-24 design
// spike; re-verified on this host for the skeleton row). Catalog snapshot:
//
//   gpt-5.6-sol          default=low     levels=low,medium,high,xhigh,max,ultra
//   gpt-5.6-terra        default=medium  levels=low,medium,high,xhigh,max,ultra
//   gpt-5.6-luna         default=medium  levels=low,medium,high,xhigh,max
//   gpt-5.5              default=medium  levels=low,medium,high,xhigh
//   gpt-5.4 / gpt-5.4-mini               levels=low,medium,high,xhigh
//   gpt-5.3-codex-spark  default=high    levels=low,medium,high,xhigh
//   codex-auto-review    (hidden)        levels=low,medium,high,xhigh
//
// `ModelMenu` is static function pointers today, so this is a baked snapshot
// rather than a live `codex debug models` parse. Per-model effort filtering
// (only expose rungs the *selected* model supports) is follow-on work under
// the ModelAndEffortMenu gap — Boss's five [`EffortLevel`]s already fit
// inside every listed model's ladder, and `ultra` has no [`EffortLevel`] to
// map from (see [`ModelMenu::effort_value_for_level`]).

/// Map a Boss effort level onto Codex's reasoning-effort vocabulary.
///
/// Mirrors Claude's five-rung ladder so operator-facing effort names stay
/// consistent across drivers. Codex's sixth rung (`ultra` on `gpt-5.6-sol` /
/// `gpt-5.6-terra`) is unreachable through [`EffortLevel`] by design.
fn codex_effort_value_for_level(level: EffortLevel) -> Option<&'static str> {
    Some(match level {
        EffortLevel::Trivial => "low",
        EffortLevel::Small => "medium",
        EffortLevel::Medium => "high",
        EffortLevel::Large => "xhigh",
        EffortLevel::Max => "max",
    })
}

/// Capability-lever model choice. `terra` is the well-articulated coding tier;
/// `sol` is the frontier model reserved for investigation/design work —
/// analogous to Claude's sonnet/opus split.
fn codex_model_for_reasoning(reasoning: ReasoningMode) -> &'static str {
    match reasoning {
        ReasoningMode::Standard => "gpt-5.6-terra",
        ReasoningMode::Investigation => "gpt-5.6-sol",
    }
}

/// Legacy size-derived table. Consulted only for rows with no
/// [`ReasoningMode`]. Keeps untagged rows on the frontier default rather than
/// inventing a size→model progression Codex has not validated.
fn codex_default_model_for_level(_level: EffortLevel) -> &'static str {
    "gpt-5.6-sol"
}

fn codex_prompt_addendum_for_level(level: EffortLevel) -> Option<&'static str> {
    match level {
        EffortLevel::Trivial | EffortLevel::Small => None,
        EffortLevel::Medium => Some("Sketch a brief plan before you start editing."),
        EffortLevel::Large | EffortLevel::Max => Some(
            "Begin with a written plan. Identify the files you expect to touch and the \
             order you'll touch them in. Confirm the approach against the work item's \
             description before writing code.",
        ),
    }
}

/// Codex has no Claude-style "auto permissions" model family. Always `false`.
fn codex_model_requires_auto_permissions(_model: &str) -> bool {
    false
}

static CODEX_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "codex",
    label: "OpenAI Codex",
    binary: "codex",
    config_dir: ".codex",
    agent_rules_filename: "AGENTS.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        // Highest-priority model in `codex debug models` (0.145.0): frontier
        // agentic coding. Step-5 fall-through only — classified rows resolve
        // through `model_for_reasoning`.
        engine_default: "gpt-5.6-sol",
        effort_value_for_level: codex_effort_value_for_level,
        default_model_for_level: codex_default_model_for_level,
        model_for_reasoning: codex_model_for_reasoning,
        prompt_addendum_for_level: codex_prompt_addendum_for_level,
        model_requires_auto_permissions: codex_model_requires_auto_permissions,
    },
};

/// Preamble for the agent-rules file (`AGENTS.md`). Names Codex observability
/// rather than Claude hooks so the shared body below it is not lying about
/// the mechanism this session uses.
const CODEX_AGENT_RULES_PREAMBLE: &str = "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via the Codex `exec --json` event stream.\n\
     For ordinary pre-push validation, run `checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.";

/// OpenAI Codex CLI driver skeleton.
///
/// Registered under the `"codex"` slug. Declares the v1 capability set from
/// the Codex driver design; behavioural spawn/provision/normalise methods are
/// follow-on rows and refuse explicitly rather than silent-no-op.
pub struct CodexDriver;

#[async_trait]
impl AgentDriver for CodexDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &CODEX_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Capability declaration for CodexDriver (v1) — design doc §Capability
        // declaration. Every omission below is deliberate; each notes its
        // absence disposition and why.
        //
        // Provided (all except ToolProvisioning + AwaitingInputSignal):
        //   Spawn, WorkspaceProvisioning, PermissionPolicy, ModelAndEffortMenu,
        //   ProgressObservation, ToolUseInterception (deny-only), TurnBoundary,
        //   StructuredOutput, TranscriptAccess, ControlVerbs, PromptComposition.
        //
        // ToolUseInterception is **deny-only**: Codex PreToolUse accepts
        // `permissionDecision: deny` but rejects `allow` / `ask` / `updatedInput`
        // (verified codex-cli 0.145.0). The trait rewrite path is unreachable;
        // inline-`--body` editorial cases become Deny-with-reason (spawn row).
        // Declaration is honest once hook trust provisioning lands —
        // untrusted hooks fail open silently, so the capability is gated on
        // that investigation before the first real Codex worker runs.
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
            // nothing). Codex has MCP/plugins/skills but Boss injects none;
            // declaring it would overclaim. Degrade is correct: no dispatch
            // refusal, no synthesised tooling.
            //
            // AwaitingInputSignal — omitted → default Degrade (never
            // Synthesize). `codex exec` is one turn per process; `turn.completed`
            // means exit is imminent, not "blocked on a human". There is no
            // channel that positively means awaiting-input (agent-driver design
            // §Decision: AwaitingInput derivation; codex-progress-channel-
            // decision investigation).
        ])
    }

    fn spawn_invocation(&self, _request: SpawnRequest<'_>) -> SpawnPlan {
        // Follow-on: `codex exec --json --strict-config … </dev/null` plus
        // CODEX_HOME. Must pass `assert_codex_exec_spawn_contract` when
        // implemented.
        unimplemented!(
            "CodexDriver::spawn_invocation is the Codex spawn/provisioning follow-on; \
             skeleton declares Spawn but does not build the exec line yet"
        )
    }

    async fn provision_workspace(
        &self,
        _workspace: &Path,
        _prompt_text: &str,
        _run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>> {
        // Follow-on: per-run CODEX_HOME, auth.json symlink, AGENTS.md, project
        // trust stamp, external_config_migration_prompts disabled. Returns the
        // Boss-owned CODEX_HOME path as DriverRuntimeState for teardown.
        unimplemented!(
            "CodexDriver::provision_workspace is the Codex spawn/provisioning follow-on; \
             skeleton declares WorkspaceProvisioning but does not materialise CODEX_HOME yet"
        )
    }

    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        // Until provision_workspace returns a CODEX_HOME payload, there is
        // nothing out-of-workspace to clean. Honour the trait contract: None
        // → no-op (do not invent a cleanup target by scanning ~/.codex). If a
        // payload is somehow present before teardown is implemented, refuse
        // rather than silently drop it.
        match runtime_state {
            None => Ok(()),
            Some(_) => unimplemented!(
                "CodexDriver::teardown_workspace received DriverRuntimeState but CODEX_HOME \
                 cleanup is not implemented yet (lands with spawn/provisioning)"
            ),
        }
    }

    async fn write_permission_config(
        &self,
        _input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        // Follow-on: `--sandbox <mode>`, CODEX_HOME env, writable_roots config,
        // and deny-only PreToolUse hook TOML (gated on hook trust provisioning).
        unimplemented!(
            "CodexDriver::write_permission_config is the Codex spawn/provisioning follow-on; \
             skeleton declares PermissionPolicy but does not render sandbox/hooks artifacts yet"
        )
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // Codex `--json` carries `item.started` / `item.completed` around each
        // tool call — same per-tool resolution as Claude's hooks (Progress-
        // Observation gap / ProgressFidelity docs). Tier is about resolution,
        // not transport.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, _config: &ProgressObservationConfig) -> ProgressIngress {
        // Progress channel is stdout JSONL (`codex exec --json`), not hooks
        // (codex-progress-channel-decision-2026-07-24). Hooks remain the
        // ToolUseInterception transport only.
        //
        // **Transport premise (ghostty pane-viability Q1):** `StdoutJsonl` is
        // valid only when the engine (or an observer that owns the stream)
        // holds the worker's stdout — e.g. engine-spawned `codex exec --json`
        // with a pipe, or a file channel such as the rollout. An outsider
        // process that holds only `shell_pid` under an app-owned Ghostty pty
        // reads zero bytes (`tools/boss/docs/investigations/ghostty-codex-
        // pane-viability.md` Q1). Do not assume pane attach hands the engine
        // a readable stdout stream. Spawn wiring must attach a real
        // engine-owned reader (or app-forward path), not `shell_pid` alone.
        ProgressIngress::StdoutJsonl
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        // Progress-normaliser follow-on implements the real mapping
        // (thread.started / turn.* / item.*). Refuse every envelope explicitly
        // so a premature stdout-ingress wire-up degrades to "driver could not
        // decode" counters rather than panicking or inventing Claude-shaped
        // events.
        let kind = raw.get("type").and_then(|v| v.as_str()).unwrap_or("<missing type>");
        Err(NormalizeError::UnknownEvent(format!(
            "{kind} (CodexDriver progress normaliser is not implemented yet)"
        )))
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // Once the progress normaliser maps `turn.completed` →
        // `WorkerEvent::Stop`, the boundary is the same shape as Claude's:
        // Stop means the turn ended. `codex exec` does not re-enter via
        // stop-hooks, so continuation is always false. Implementing this now
        // is correct and trivial; it simply never fires until the normaliser
        // lands.
        match event {
            WorkerEvent::Stop {
                session_id,
                stop_reason,
                ..
            } => Some(TurnEnd {
                session_id: session_id.clone(),
                reason: *stop_reason,
                continuation: false,
            }),
            _ => None,
        }
    }

    fn tool_use_interception_wiring(&self, _config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        // Declared deny-only (see capabilities). Real wiring emits Boss guard
        // scripts into CODEX_HOME `[[hooks.PreToolUse]]` TOML and stamps
        // trusted_hash (hook trust + spawn/provisioning). Returning empty
        // hooks here would be a silent no-op of every guardrail — forbidden
        // while the capability is declared.
        unimplemented!(
            "CodexDriver::tool_use_interception_wiring is the spawn/provisioning follow-on \
             (deny-only PreToolUse + trust); refusing empty wiring so guards are not silently dropped"
        )
    }

    fn agent_rules_preamble(&self) -> &'static str {
        CODEX_AGENT_RULES_PREAMBLE
    }

    fn transcript_path_for_session(&self, _raw: &serde_json::Value) -> Option<String> {
        // Primary discovery is `$CODEX_HOME/sessions/…/rollout-*-<thread_id>.jsonl`
        // derived from thread.started + the provisioned home (TranscriptAccess
        // gap) — not a field on the stdout stream. That needs provision's
        // runtime state plus the normaliser's sticky thread_id. Honest None
        // until then: callers retry on a later payload / fall back.
        None
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        // Codex rollout line schema remapping is follow-on (TranscriptAccess /
        // transcript-tail generalisation). Pass-through until a real remapper
        // lands — identity is wrong for Codex shapes but does not invent data;
        // the live-status path already tolerates unrecognised entries.
        raw
    }

    fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
        // Codex-specific API-error shapes are not extracted yet (ControlVerbs
        // hardening). None is the honest "no recognised halting error" answer,
        // not a claim that the run was clean.
        None
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        // Must not route through `classify_claude_error`. Real Codex
        // classification (rate limits, quota, auth) is ControlVerbs follow-on.
        // Indeterminate is the documented "recognised as an error but not
        // confidently bucketed; treat as Permanent" class — explicit, not a
        // silent Transient that would auto-resume a permanent failure.
        WorkerErrorClass::Indeterminate
    }

    fn structured_output_wiring(
        &self,
        request: &StructuredOutputRequest<'_>,
    ) -> anyhow::Result<StructuredOutputArtifacts> {
        // Common-denominator env-file contract works for Codex today. Spawn /
        // StructuredOutput follow-on can extend this with `--output-schema` /
        // `--output-last-message` on top of the same path (prefer starting
        // from the default).
        Ok(default_structured_output_wiring(request))
    }

    fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
        // No Codex-specific prose-scrape conventions yet. Empty Vec is the
        // honest answer: primary channel is the file contract (+ future
        // --output-schema), not transcript scraping.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AbsenceDisposition, Capability};
    use boss_protocol::StopReason;

    #[test]
    fn codex_descriptor_matches_design() {
        let driver = CodexDriver;
        let d = driver.descriptor();
        assert_eq!(d.name, "codex");
        assert_eq!(d.label, "OpenAI Codex");
        assert_eq!(d.binary, "codex");
        assert_eq!(d.config_dir, ".codex");
        assert_eq!(d.agent_rules_filename, "AGENTS.md");
        assert_eq!(d.initial_prompt_filename, "initial-prompt.txt");
        assert_eq!(d.model_menu.engine_default, "gpt-5.6-sol");
    }

    #[test]
    fn codex_model_menu_sourced_from_debug_models_vocabulary() {
        let menu = &CodexDriver.descriptor().model_menu;
        // Effort values are the codex debug models ladder (low..max).
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Trivial), Some("low"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Small), Some("medium"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Medium), Some("high"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Large), Some("xhigh"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Max), Some("max"));
        // Reasoning split uses catalog slugs from debug models.
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Standard), "gpt-5.6-terra");
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Investigation), "gpt-5.6-sol");
        assert!(!(menu.model_requires_auto_permissions)("gpt-5.6-sol"));
    }

    #[test]
    fn codex_declares_design_capability_set() {
        let caps = CodexDriver.capabilities();
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
            assert!(caps.provides(cap), "CodexDriver must provide {cap:?}");
        }
        // Explicit omissions.
        assert!(
            !caps.provides(Capability::ToolProvisioning),
            "ToolProvisioning is unused in v1; omit → Degrade"
        );
        assert!(
            !caps.provides(Capability::AwaitingInputSignal),
            "codex exec has no awaiting-input signal; omit → Degrade"
        );
        assert_eq!(
            caps.absence_disposition(Capability::ToolProvisioning),
            AbsenceDisposition::Degrade
        );
        assert_eq!(
            caps.absence_disposition(Capability::AwaitingInputSignal),
            AbsenceDisposition::Degrade
        );
    }

    #[test]
    fn codex_progress_ingress_is_stdout_jsonl() {
        let config = ProgressObservationConfig {
            events_socket_path: std::path::PathBuf::from("/tmp/events.sock"),
            lease_id: "lease".into(),
            run_id: "run".into(),
            workspace_path: std::path::PathBuf::from("/ws"),
            forwarder_binary: std::path::PathBuf::from("/bin/boss-event"),
        };
        match CodexDriver.progress_observation_wiring(&config) {
            ProgressIngress::StdoutJsonl => {}
            ProgressIngress::HookCallback(_) => panic!("Codex progress is StdoutJsonl, not hooks"),
        }
        assert_eq!(CodexDriver.progress_fidelity(), ProgressFidelity::Rich);
    }

    #[test]
    fn codex_turn_boundary_on_stop_is_non_continuation() {
        let event = WorkerEvent::Stop {
            session_id: "thread-1".into(),
            stop_hook_active: true, // ignored for Codex
            stop_reason: StopReason::Completed,
        };
        let boundary = CodexDriver.turn_boundary(&event).expect("Stop is a boundary");
        assert_eq!(boundary.session_id, "thread-1");
        assert_eq!(boundary.reason, StopReason::Completed);
        assert!(!boundary.continuation);
        assert!(
            CodexDriver
                .turn_boundary(&WorkerEvent::SessionStart {
                    session_id: "thread-1".into(),
                    source: boss_protocol::SessionStartSource::Startup,
                })
                .is_none()
        );
    }

    #[test]
    fn normalize_progress_event_refuses_until_normaliser_lands() {
        let raw = serde_json::json!({"type": "turn.completed"});
        let err = CodexDriver
            .normalize_progress_event(&raw)
            .expect_err("normaliser is not implemented yet");
        match err {
            NormalizeError::UnknownEvent(msg) => {
                assert!(msg.contains("turn.completed"), "{msg}");
                assert!(msg.contains("not implemented"), "{msg}");
            }
            other => panic!("expected UnknownEvent, got {other:?}"),
        }
    }
}
