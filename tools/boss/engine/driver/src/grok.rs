//! `GrokDriver` — xAI Grok Build agent driver.
//!
//! Descriptor, capability set, model menu, Boss-owned `GROK_HOME` provisioning,
//! interactive-TUI pane spawn, and hook wiring (progress forwarder + the five
//! `PreToolUse` guards behind a canonicalisation adapter — design T-09, see
//! [`hooks`]). Full permission-policy artifacts (`--sandbox`/`--allow`/`--deny`,
//! T-17), control verbs, and transcript access remain follow-on work — see
//! `tools/boss/docs/designs/grok-as-a-first-class-interactive-agent-driver.md`.
//!
//! Behavioural fan-out lives under the `grok/` submodule directory so
//! concurrent follow-on tasks (transcript, control verbs, output capture,
//! characterisation) do not serialise on this file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{NormalizeError, PaneMonitorSpec, WorkerEvent};
use boss_ssh_transport::shell_quote;
use serde_json::{Value, json};

mod home;
mod hooks;
mod model_menu;
mod permissions;
mod progress;

pub use home::{
    COMPAT_SURFACES, COMPAT_VENDORS, GROK_AUTH_SOURCE_ENV, GROK_HOMES_ENV_TEST_LOCK, GROK_HOMES_ROOT_ENV,
    GROK_SKIP_POSTURE_ASSERT_ENV, GrokRuntimeState, PINNED_GROK_VERSION, assert_inspect_json_posture,
    grok_home_for_run, grok_homes_root, process_home_for_run, render_base_config_toml, trust_path_variants,
};

use home::{assert_grok_home_safe_to_delete, provision_grok_home, read_session_id, read_workspace_path_stamp};
use progress::GrokProgressSession;

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, EnvDirective, HookWiringDestination,
    ModelMenu, PermissionArtifacts, PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig,
    ProgressObservationWiring, ProgressSessionNormalizer, SpawnPlan, SpawnRequest, ToolUseInterceptionConfig,
    ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass, WorkerKind,
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
    // Boss worker rules go to `$GROK_HOME/AGENTS.md` (global scope) via
    // [`GrokDriver::agent_rules_destination`] so they do not overwrite the
    // repo's tracked `AGENTS.md`.
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
/// the mechanism this session uses.
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
/// the Grok driver design. Workspace provisioning + pane spawn are live;
/// progress wiring, full permission artifacts, and control verbs remain
/// follow-on work.
#[derive(Default)]
pub struct GrokDriver {
    // Keep this type non-unit so callers can use `Default` uniformly with
    // stateful drivers without tripping clippy's unit-default lint.
    _private: (),
}

/// Build the interactive TUI command line for a GhosttyKit pane.
///
/// Execution shape (design §Execution shape + T-03 pane mode):
/// ```text
/// grok --model … --reasoning-effort … --no-alt-screen --always-approve
///      --trust --session-id <uuid> --cwd <ws> --no-subagents --no-memory
///      "$(cat .grok/initial-prompt.txt)"
/// ```
///
/// `GROK_HOME` / scoped `HOME` / `Unset(GROK_FOLDER_TRUST)` are env
/// directives on the [`SpawnPlan`], not flags. Never emits `-w` /
/// `--worktree` / `--worktree-ref`. Never *sets* `GROK_FOLDER_TRUST=0`
/// (spawn unsets the var so a host export cannot ungate project hooks/MCP).
pub fn build_grok_pane_command(request: &SpawnRequest<'_>, workspace: &Path, session_id: &str) -> String {
    let SpawnRequest {
        model,
        effort,
        settings_path: _,
        non_opus_auto_mode: _,
        permission_mode_override: _,
        run_id: _,
    } = request;

    let mut cmd = String::from("grok");
    cmd.push_str(" --model ");
    cmd.push_str(&shell_quote(model));
    if let Some(e) = effort {
        cmd.push_str(" --reasoning-effort ");
        cmd.push_str(&shell_quote(e));
    }
    // T-03: `--no-alt-screen` is the recommended pane mode (stable
    // Esc:cancel markers; chrome retained after /quit).
    cmd.push_str(" --no-alt-screen");
    cmd.push_str(" --always-approve");
    // Hidden from --help (D-3) but required; trusted_folders.toml is the belt.
    cmd.push_str(" --trust");
    cmd.push_str(" --session-id ");
    cmd.push_str(&shell_quote(session_id));
    cmd.push_str(" --cwd ");
    cmd.push_str(&shell_quote(&workspace.display().to_string()));
    // Explicit v1 posture, not a pane-usability or lifecycle blocker:
    // Boss injects none of Grok's MCP/plugin/skill/subagent surface, and
    // the driver disables what it does not use rather than inheriting
    // defaults — a subagent is state Boss does not model (design
    // `grok-as-a-first-class-interactive-agent-driver.md` G-11 / T-07).
    // Claude declares no equivalent flag because its subagents emit
    // through the hook stream Boss already reads; whether a Grok subagent
    // picks up the global `$GROK_HOME/hooks/` set (so its tool calls are
    // intercepted and its turns attributed) is unmeasured, so lifting
    // this needs a probe first.
    cmd.push_str(" --no-subagents");
    cmd.push_str(" --no-memory");
    // Prompt from file via command substitution — briefs run to tens of KB.
    cmd.push_str(&format!(
        " \"$(cat {}/{})\"\n",
        GROK_DESCRIPTOR.config_dir, GROK_DESCRIPTOR.initial_prompt_filename,
    ));
    cmd
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

    fn pane_monitor_spec(&self) -> Option<PaneMonitorSpec> {
        // Measured under GhosttyKit with `--no-alt-screen` (recommended
        // pane mode). See
        // `tools/boss/docs/investigations/grok-tui-liveness-markers-under-ghosttykit.md`.
        // Do NOT merge these into Claude's marker sets — each driver
        // owns its surface strings.
        Some(PaneMonitorSpec {
            // OR-semantics. "always-approve" assumes Boss spawn keeps
            // --always-approve. "Grok 4" matches footer "Grok 4.5 …"
            // without pinning the patch model id.
            agent_markers: vec!["Shift+Tab:mode".into(), "always-approve".into(), "Grok 4".into()],
            // Footer affordance present on every busy poll, absent idle.
            busy_markers: vec!["Esc:cancel".into(), "[stop]".into()],
            // Prefix matches "Starting session…" (unicode ellipsis) too.
            starting_markers: vec!["Starting session".into()],
            // Boxed composer only — bare "❯" collides with history
            // user-message lines (`     ❯ Use the shell…`).
            prompt_prefixes: vec!["│ ❯".into()],
            idle_debounce_polls: 2,
        })
    }

    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan {
        let run_id = request.run_id.filter(|id| !id.is_empty()).unwrap_or("unknown-run");
        let grok_home =
            grok_home_for_run(run_id).unwrap_or_else(|_| grok_homes_root().join("unknown-run").join("grok-home"));
        let process_home =
            process_home_for_run(run_id).unwrap_or_else(|_| grok_homes_root().join("unknown-run").join("process-home"));

        // Session id + workspace path written by provision_workspace.
        // Fallbacks are for fixtures that skip provision only.
        let session_id = read_session_id(&grok_home).unwrap_or_else(|_| {
            home::new_session_uuid().unwrap_or_else(|_| "00000000-0000-4000-8000-000000000000".to_owned())
        });
        let workspace_for_cwd = read_workspace_path_stamp(&grok_home).unwrap_or_else(|_| PathBuf::from("."));

        let command = build_grok_pane_command(&request, &workspace_for_cwd, &session_id);

        // Defence-in-depth: never emit worktree flags (cube owns workspaces).
        debug_assert!(!command.contains("--worktree") && !command.contains(" -w ") && !command.contains("\t-w "));

        SpawnPlan {
            env: vec![
                EnvDirective::Set("GROK_HOME".to_owned(), grok_home.display().to_string()),
                // T-01: scope HOME so operator ~/.claude settings cannot load.
                // Auth stays under GROK_HOME (symlink), not under this HOME.
                //
                // Deferred (seeded first-turn scope): scoped HOME also hides
                // operator ~/.ssh, ~/.config/gh, and git credential helpers
                // from the worker after the first turn. Full credential-tree
                // bridging (selective symlink/copy of gh/ssh/git material into
                // process-home) is out of scope for this provision/spawn
                // slice — track as a follow-on before multi-turn Grok workers
                // that need authenticated network/VCS.
                EnvDirective::Set("HOME".to_owned(), process_home.display().to_string()),
                // Never inherit host GROK_FOLDER_TRUST=0 — that value ungates
                // project hooks/MCP and undoes the config.toml disable block.
                // Mirror the inspect path's env_remove on the pane shell.
                EnvDirective::Unset("GROK_FOLDER_TRUST".to_owned()),
            ],
            command,
        }
    }

    async fn provision_workspace(
        &self,
        workspace: &Path,
        prompt_text: &str,
        run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>> {
        let runtime = provision_grok_home(workspace, prompt_text, run_id)
            .with_context(|| format!("provisioning Boss-owned GROK_HOME for run_id {run_id:?}"))?;
        Ok(Some(runtime.to_driver_runtime_state()))
    }

    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        let Some(state) = runtime_state else {
            // No payload → no-op. Do not invent a cleanup target.
            return Ok(());
        };
        let runtime = GrokRuntimeState::from_driver_runtime_state(state)?;
        // Containment check even though we do not delete here: a tampered
        // payload must surface loudly. Home is retained as run evidence
        // (same retention posture as Codex); reclaim is a follow-on policy.
        assert_grok_home_safe_to_delete(&runtime.grok_home)?;
        Ok(())
    }

    /// Write the `$GROK_HOME` hook wiring (design T-09): the `boss-event`
    /// progress forwarder on every lifecycle event, plus the five
    /// `PreToolUse` guards behind [`hooks::write_hooks`]'s canonicalisation
    /// adapter. Overwrites the provisional canary `provision_workspace`
    /// wrote.
    ///
    /// Also writes the full permission-policy artifacts (design T-17): a
    /// Boss-owned `$GROK_HOME/sandbox.toml` custom profile (kernel-level deny
    /// of the Boss data dir, layered on the worker kind's built-in
    /// `workspace`/`read-only` base — omitted for remote workers, which have
    /// no local data dir to fence), and `extra_args` carrying `--sandbox`,
    /// one `--deny` per structural rule (Boss data dir, `rm -rf`, `sudo`,
    /// `bossctl`), and `--permission-mode` when the worker kind forces one.
    /// See [`permissions`] for the profile/rule rendering and
    /// `grok-permission-isolation-2026-07-27.md` (T-16) for the grammar this
    /// relies on.
    async fn write_permission_config(
        &self,
        input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        let grok_home = grok_home_for_run(&input.run_id).with_context(|| {
            format!(
                "GrokDriver::write_permission_config: resolving GROK_HOME for run_id {:?}",
                input.run_id
            )
        })?;
        if !grok_home.exists() {
            bail!(
                "GrokDriver::write_permission_config: GROK_HOME {} does not exist; \
                 call provision_workspace first",
                grok_home.display()
            );
        }

        let obs_config = ProgressObservationConfig {
            events_socket_path: input.events_socket_path.clone(),
            lease_id: input.lease_id.clone(),
            run_id: input.run_id.clone(),
            workspace_path: input.workspace_path.clone(),
            forwarder_binary: input.boss_event_path.clone(),
        };
        // Mirrors CodexDriver::write_permission_config's construction of
        // ToolUseInterceptionConfig from PermissionInput exactly — remote
        // workers get no local sandbox/guard scripts (never shipped there).
        let interception = ToolUseInterceptionConfig {
            data_dir: if input.is_remote {
                None
            } else {
                input.events_socket_path.parent().map(|p| p.to_path_buf())
            },
            path_guard_script: if input.is_remote {
                None
            } else {
                input.path_guard_script.clone()
            },
            checkleft_guard_script: if input.is_remote {
                None
            } else {
                input.checkleft_guard_script.clone()
            },
            is_revision: input.execution_kind == "revision_implementation"
                || input.task_kind.as_deref() == Some("revision"),
            is_standard_worker: input.worker_kind == WorkerKind::Standard,
            run_id: Some(input.run_id.clone()),
            workspace_path: Some(input.workspace_path.clone()),
        };

        let mut config_files = hooks::write_hooks(&grok_home, &obs_config, &interception)
            .with_context(|| format!("writing Grok hook wiring under {}", grok_home.display()))?;
        config_files.push(grok_home.join("config.toml"));

        // Boss data dir for the sandbox's kernel-level deny and the CLI
        // Read/Edit belt — `None` on remote (see `interception.data_dir` above).
        let boss_data_dir = if input.is_remote {
            None
        } else {
            input.events_socket_path.parent().map(|p| p.to_path_buf())
        };

        if !input.is_remote {
            let sandbox_toml_path = grok_home.join("sandbox.toml");
            fs::write(
                &sandbox_toml_path,
                permissions::render_sandbox_toml(input.worker_kind, boss_data_dir.as_deref()),
            )
            .with_context(|| format!("writing {}", sandbox_toml_path.display()))?;
            config_files.push(sandbox_toml_path);
        }

        let extra_args = permissions::extra_args(input.worker_kind, boss_data_dir.as_deref(), input.is_remote);

        Ok(PermissionArtifacts {
            config_files,
            extra_args,
            env: vec![("GROK_HOME".into(), grok_home.display().to_string())],
        })
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // Design G-5: Rich — per-tool PreToolUse/PostToolUse events, same
        // tier as Claude. Declared even before full wiring lands so the
        // stale-worker sweep threshold is correct once a Grok worker runs.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress {
        // Destination is DriverOwned: `write_permission_config` (via
        // `hooks::write_hooks`) is what actually writes this wiring to
        // `$GROK_HOME/hooks/` — the engine must never merge it into the
        // Claude worker settings file the Grok agent never reads (design
        // G-5 destination hazard). The hooks map returned here mirrors what
        // gets written so this capability's declared wiring reflects
        // reality rather than an empty placeholder.
        ProgressIngress::HookCallback(ProgressObservationWiring {
            hooks: hooks::forwarder_hooks_map(config),
            destination: HookWiringDestination::DriverOwned,
        })
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        // Stateless compatibility path for direct callers — mirrors
        // CodexDriver's own comment: this driver's hook payload is fully
        // self-describing per invocation, so a fresh, stateless
        // GrokProgressSession is equivalent to a durable one (see that
        // type's doc comment in `grok/progress.rs`).
        GrokProgressSession::new().normalize_progress_event(raw)
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // Design G-7: Grok's `Stop` hook maps directly onto
        // `WorkerEvent::Stop`, structurally identical to Claude's —
        // `stopHookActive` canonicalises to `stop_hook_active`, i.e.
        // `TurnEnd::continuation`. Recovering a turn boundary for an
        // Esc-interrupted turn (which skips the `Stop` hook entirely) is a
        // separate concern (design T-12), not this event mapping.
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
        // Dead in practice for Grok: `progress_observation_wiring` declares
        // `HookWiringDestination::DriverOwned`, so `settings_value`
        // (`core/src/worker_setup.rs`) never calls this method — it only
        // layers interception guards into the settings file for
        // `WorkerSettingsFile`-destination drivers. The real Grok guard
        // wiring lives in `hooks::write_hooks`, called from
        // `write_permission_config`, where the materialised
        // path-guard/checkleft-guard script paths are actually available.
        // Kept empty (not `unimplemented!()`) so a future refactor that
        // *does* start calling this generically fails safe rather than
        // panicking.
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }

    fn agent_rules_preamble(&self) -> &'static str {
        GROK_AGENT_RULES_PREAMBLE
    }

    /// Route Boss worker rules to `$GROK_HOME/AGENTS.md` (global scope).
    ///
    /// Grok reads project instructions from the workspace-root `AGENTS.md`
    /// and global rules from `$GROK_HOME/AGENTS.md` — never from
    /// `.grok/AGENTS.md`. Writing under the workspace root would clobber the
    /// repo's tracked file; writing under `.grok/` would be unread.
    fn agent_rules_destination(&self, _workspace: &Path, run_id: &str) -> PathBuf {
        grok_home_for_run(run_id)
            .unwrap_or_else(|_| grok_homes_root().join("unknown-run").join("grok-home"))
            .join(GROK_DESCRIPTOR.agent_rules_filename)
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
    use std::fs;
    use std::sync::Mutex;
    use tempfile::TempDir;

    static ENV_LOCK: &Mutex<()> = &GROK_HOMES_ENV_TEST_LOCK;

    fn spawn_request<'a>(model: &'a str, run_id: &'a str) -> SpawnRequest<'a> {
        SpawnRequest {
            model,
            effort: Some("high"),
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id: Some(run_id),
        }
    }

    /// Point homes + auth at a temp tree; skip live inspect when requested.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior_homes: Option<std::ffi::OsString>,
        prior_auth: Option<std::ffi::OsString>,
        prior_skip: Option<std::ffi::OsString>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: lock held for the lifetime of this guard.
            restore(GROK_HOMES_ROOT_ENV, self.prior_homes.as_ref());
            restore(GROK_AUTH_SOURCE_ENV, self.prior_auth.as_ref());
            restore(GROK_SKIP_POSTURE_ASSERT_ENV, self.prior_skip.as_ref());
        }
    }

    fn restore(key: &str, prior: Option<&std::ffi::OsString>) {
        match prior {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }

    fn env_for_provision(homes: &Path, auth_src: &Path, skip_inspect: bool) -> EnvGuard {
        let lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let prior_homes = std::env::var_os(GROK_HOMES_ROOT_ENV);
        let prior_auth = std::env::var_os(GROK_AUTH_SOURCE_ENV);
        let prior_skip = std::env::var_os(GROK_SKIP_POSTURE_ASSERT_ENV);
        // SAFETY: serialised by ENV_LOCK.
        unsafe {
            std::env::set_var(GROK_HOMES_ROOT_ENV, homes);
            std::env::set_var(GROK_AUTH_SOURCE_ENV, auth_src);
            if skip_inspect {
                std::env::set_var(GROK_SKIP_POSTURE_ASSERT_ENV, "1");
            } else {
                std::env::remove_var(GROK_SKIP_POSTURE_ASSERT_ENV);
            }
        }
        EnvGuard {
            _lock: lock,
            prior_homes,
            prior_auth,
            prior_skip,
        }
    }

    fn write_fake_auth(path: &Path) {
        fs::write(
            path,
            r#"{"token":"test-only-not-a-real-credential","provider":"grok.com"}"#,
        )
        .unwrap();
    }

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
    fn grok_pane_monitor_spec_matches_ghosttykit_investigation() {
        let spec = GrokDriver::default()
            .pane_monitor_spec()
            .expect("GrokDriver supplies pane-monitor markers");
        assert_eq!(spec.agent_markers, vec!["Shift+Tab:mode", "always-approve", "Grok 4"]);
        assert_eq!(spec.busy_markers, vec!["Esc:cancel", "[stop]"]);
        assert_eq!(spec.starting_markers, vec!["Starting session"]);
        // Boxed composer only — bare ❯ collides with history lines.
        assert_eq!(spec.prompt_prefixes, vec!["│ ❯"]);
        assert_eq!(spec.idle_debounce_polls, 2);
        // Guardrail: never smuggle Claude busy chrome into Grok's set.
        assert!(!spec.busy_markers.iter().any(|m| m.contains("esc to interrupt")));
        assert!(!spec.agent_markers.iter().any(|m| m.contains("Claude")));
    }

    #[test]
    fn grok_progress_fidelity_is_rich() {
        assert_eq!(GrokDriver::default().progress_fidelity(), ProgressFidelity::Rich);
    }

    #[test]
    fn progress_observation_is_driver_owned_not_settings_merge() {
        let ingress = GrokDriver::default().progress_observation_wiring(&ProgressObservationConfig {
            events_socket_path: PathBuf::from("/tmp/events.sock"),
            lease_id: "lease".into(),
            run_id: "run".into(),
            workspace_path: PathBuf::from("/tmp/ws"),
            forwarder_binary: PathBuf::from("/tmp/boss-event"),
        });
        match ingress {
            ProgressIngress::HookCallback(w) => {
                assert_eq!(w.destination, HookWiringDestination::DriverOwned);
                // Destination is DriverOwned, so the engine never merges this
                // into the settings file regardless of content (see
                // `merges_hooks_into_worker_settings` in
                // `core/src/worker_setup.rs`) — but the map itself should
                // reflect the real forwarder wiring `write_permission_config`
                // installs, not an empty placeholder.
                assert!(!w.hooks.is_empty(), "hooks map must mirror the real forwarder wiring");
                assert!(w.hooks.contains_key("PreToolUse"));
                assert!(w.hooks.contains_key("Stop"));
            }
            other => panic!("expected HookCallback(DriverOwned), got {other:?}"),
        }
    }

    #[test]
    fn spawn_invocation_matches_execution_shape() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        // Seed session id file as provision would.
        let run_id = "run-spawn-1";
        let grok_home = grok_home_for_run(run_id).unwrap();
        fs::create_dir_all(&grok_home).unwrap();
        fs::write(
            grok_home.join("boss-session-id"),
            "11111111-2222-4333-8444-555555555555\n",
        )
        .unwrap();
        fs::write(grok_home.join("boss-workspace-path"), "/tmp/ws-spawn-test\n").unwrap();

        let plan = GrokDriver::default().spawn_invocation(spawn_request("grok-4.5", run_id));

        assert!(
            plan.env.iter().any(|d| matches!(
                d,
                EnvDirective::Set(k, v) if k == "GROK_HOME" && v.contains("run-spawn-1")
            )),
            "must export GROK_HOME: {:?}",
            plan.env
        );
        assert!(
            plan.env
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, _) if k == "HOME")),
            "must export scoped HOME (T-01): {:?}",
            plan.env
        );
        assert!(
            plan.env
                .iter()
                .any(|d| matches!(d, EnvDirective::Unset(k) if k == "GROK_FOLDER_TRUST")),
            "must Unset GROK_FOLDER_TRUST so ambient host=0 cannot ungate hooks/MCP: {:?}",
            plan.env
        );
        assert!(
            !plan.env.iter().any(|d| matches!(
                d,
                EnvDirective::Set(k, v) if k == "GROK_FOLDER_TRUST" && v == "0"
            )),
            "must never set GROK_FOLDER_TRUST=0: {:?}",
            plan.env
        );

        let cmd = &plan.command;
        assert!(cmd.starts_with("grok "), "command starts with grok: {cmd}");
        assert!(cmd.contains("--model "), "has --model: {cmd}");
        assert!(cmd.contains("grok-4.5"), "has model slug: {cmd}");
        assert!(cmd.contains("--reasoning-effort "), "has --reasoning-effort: {cmd}");
        assert!(cmd.contains("high"), "has effort value: {cmd}");
        assert!(cmd.contains("--no-alt-screen"), "T-03 pane mode: {cmd}");
        assert!(cmd.contains("--always-approve"), "has --always-approve: {cmd}");
        assert!(cmd.contains("--trust"), "has --trust: {cmd}");
        assert!(
            cmd.contains("--session-id ") && cmd.contains("11111111-2222-4333-8444-555555555555"),
            "has Boss session id: {cmd}"
        );
        assert!(
            cmd.contains("--cwd ") && cmd.contains("/tmp/ws-spawn-test"),
            "has absolute --cwd from provision stamp: {cmd}"
        );
        assert!(cmd.contains("--no-subagents"), "has --no-subagents: {cmd}");
        assert!(cmd.contains("--no-memory"), "has --no-memory: {cmd}");
        assert!(
            cmd.contains("\"$(cat .grok/initial-prompt.txt)\""),
            "prompt via cat substitution: {cmd}"
        );
        assert!(!cmd.contains("--worktree"), "forbids --worktree: {cmd}");
        assert!(!cmd.contains("--worktree-ref"), "forbids --worktree-ref: {cmd}");
        // Bare `-w` as a flag token (not part of another word).
        let tokens: Vec<&str> = cmd.split_whitespace().collect();
        assert!(!tokens.contains(&"-w"), "forbids bare -w: {cmd}");
    }

    #[tokio::test]
    async fn provision_workspace_creates_owned_home_symlink_auth_and_trust() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("source-auth.json");
        write_fake_auth(&auth);

        // Skip live inspect: auth is fake and this host may not have network.
        // Layout + symlink semantics are the unit under test; live inspect is
        // covered by provision_workspace_live_inspect_when_grok_present.
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let state = driver
            .provision_workspace(&workspace, "hello prompt", "run-prov-1")
            .await
            .expect("provision")
            .expect("Grok must return runtime state");

        let runtime = GrokRuntimeState::from_driver_runtime_state(&state).unwrap();
        assert!(runtime.grok_home.starts_with(&homes));
        assert!(runtime.process_home.starts_with(&homes));
        assert_ne!(runtime.grok_home, runtime.process_home);

        // Auth is a symlink, never a copy.
        let auth_dest = runtime.grok_home.join("auth.json");
        let meta = fs::symlink_metadata(&auth_dest).unwrap();
        assert!(meta.file_type().is_symlink(), "auth.json must be a symlink");
        let target = fs::read_link(&auth_dest).unwrap();
        assert_eq!(target, auth);

        let config = fs::read_to_string(runtime.grok_home.join("config.toml")).unwrap();
        assert!(config.contains("[compat.claude]"));
        assert!(config.contains("[compat.cursor]"));
        assert!(config.contains("hooks = false"));
        assert!(config.contains("mcps = false"));
        assert!(config.contains("sessions = false"));
        assert!(
            !config.lines().any(|l| {
                let t = l.trim();
                !t.starts_with('#') && t.contains("plugins")
            }),
            "plugins is not an official compat cell assignment: {config}"
        );

        let trust = fs::read_to_string(runtime.grok_home.join("trusted_folders.toml")).unwrap();
        assert!(trust.contains("trusted = true"));
        // Workspace path (and symlink forms) must appear.
        let ws_str = workspace.display().to_string();
        assert!(
            trust.contains(&ws_str) || trust_path_variants(&workspace).iter().any(|p| trust.contains(p)),
            "trust file must stamp workspace path; trust={trust}"
        );

        assert!(runtime.grok_home.join("hooks/boss-provision.json").is_file());
        assert!(!runtime.session_id.is_empty());
        assert_eq!(
            fs::read_to_string(runtime.grok_home.join("boss-session-id"))
                .unwrap()
                .trim(),
            runtime.session_id
        );

        let prompt = workspace.join(".grok/initial-prompt.txt");
        assert_eq!(fs::read_to_string(prompt).unwrap(), "hello prompt");
        assert!(workspace.join(".grok/.gitignore").is_file());

        // Interactive home must not be the GROK_HOME.
        if let Some(home) = std::env::var_os("HOME") {
            assert_ne!(runtime.grok_home, PathBuf::from(home).join(".grok"));
        }

        // Idempotent re-provision keeps the same session id.
        let state2 = driver
            .provision_workspace(&workspace, "second prompt", "run-prov-1")
            .await
            .unwrap()
            .unwrap();
        let runtime2 = GrokRuntimeState::from_driver_runtime_state(&state2).unwrap();
        assert_eq!(runtime.session_id, runtime2.session_id);
        assert_eq!(
            fs::read_to_string(workspace.join(".grok/initial-prompt.txt")).unwrap(),
            "second prompt"
        );
    }

    #[tokio::test]
    async fn provision_workspace_live_inspect_when_grok_present() {
        // Soft-skip when `grok` is not on PATH (CI hosts without the pin).
        if std::process::Command::new("grok")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("grok not available; skipping live inspect provision test");
            return;
        }
        // Need a real auth.json to satisfy symlink existence; use host only
        // as *source* for the symlink, never as GROK_HOME.
        let host_auth = dirs_home_grok_auth();
        if !host_auth.exists() {
            eprintln!("no host ~/.grok/auth.json; skipping live inspect provision test");
            return;
        }

        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let _guard = env_for_provision(&homes, &host_auth, false);

        let driver = GrokDriver::default();
        let state = driver
            .provision_workspace(&workspace, "live inspect prompt", "run-live-1")
            .await
            .expect("live provision + inspect must succeed");
        assert!(state.is_some());
    }

    fn dirs_home_grok_auth() -> PathBuf {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".grok").join("auth.json"))
            .unwrap_or_else(|| PathBuf::from(".grok/auth.json"))
    }

    #[test]
    fn agent_rules_destination_is_grok_home_not_dot_grok() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_for_provision(tmp.path(), &tmp.path().join("auth.json"), true);
        let workspace = tmp.path().join("ws");
        let dest = GrokDriver::default().agent_rules_destination(&workspace, "run-agents-1");
        assert!(
            dest.ends_with("grok-home/AGENTS.md") || dest.file_name() == Some("AGENTS.md".as_ref()),
            "dest={dest:?}"
        );
        assert!(
            !dest.components().any(|c| c.as_os_str() == ".grok"),
            "must not write under workspace .grok: {dest:?}"
        );
    }

    #[tokio::test]
    async fn write_permission_config_requires_prior_provision() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Standard,
            workspace_path: tmp.path().join("ws"),
            events_socket_path: tmp.path().join("events.sock"),
            boss_event_path: tmp.path().join("boss-event"),
            run_id: "run-no-provision".into(),
            lease_id: "lease".into(),
            execution_kind: "task_implementation".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: None,
            checkleft_guard_script: None,
        };
        let err = GrokDriver::default()
            .write_permission_config(&input, tmp.path())
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("does not exist"),
            "expected missing-home error, got {err:#}"
        );
    }

    /// End-to-end (design T-09 acceptance, driver-level slice): after
    /// `provision_workspace`, `write_permission_config` must write the real
    /// hook wiring — forwarder + adapter-wrapped guards — into
    /// `$GROK_HOME/hooks/`, not the provisional canary.
    #[tokio::test]
    async fn write_permission_config_writes_real_hook_wiring() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-1";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();

        let path_guard_script = tmp.path().join("boss-path-guard.py");
        fs::write(&path_guard_script, "#!/usr/bin/env python3\n").unwrap();
        let checkleft_guard_script = tmp.path().join("boss-checkleft-push-guard.py");
        fs::write(&checkleft_guard_script, "#!/usr/bin/env python3\n").unwrap();

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Standard,
            workspace_path: workspace.clone(),
            events_socket_path: tmp.path().join("boss-data").join("events.sock"),
            boss_event_path: tmp.path().join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-1".into(),
            execution_kind: "task_implementation".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: Some(path_guard_script),
            checkleft_guard_script: Some(checkleft_guard_script),
        };

        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();
        assert!(
            artifacts.env.iter().any(|(k, _)| k == "GROK_HOME"),
            "must surface GROK_HOME: {:?}",
            artifacts.env
        );

        let grok_home = grok_home_for_run(run_id).unwrap();
        let hooks_path = grok_home.join("hooks").join("boss-provision.json");
        let adapter_path = grok_home.join("hooks").join("boss-grok-hook-adapter.py");
        assert!(
            artifacts.config_files.contains(&hooks_path),
            "config_files must include the hooks wiring file: {:?}",
            artifacts.config_files
        );
        assert!(
            artifacts.config_files.contains(&adapter_path),
            "config_files must include the adapter script: {:?}",
            artifacts.config_files
        );

        let hooks_doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let pre_tool_use = hooks_doc["hooks"]["PreToolUse"].as_array().unwrap();
        // forwarder + path guard + boss-launch guard + pr-redirect guard + checkleft guard.
        assert_eq!(pre_tool_use.len(), 5, "{pre_tool_use:#?}");
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PostToolUse",
            "Stop",
            "Notification",
            "SessionEnd",
        ] {
            assert!(
                hooks_doc["hooks"][event].as_array().is_some_and(|a| !a.is_empty()),
                "missing forwarder wiring for {event}: {hooks_doc:#}"
            );
        }
        for entry in pre_tool_use.iter().skip(1) {
            let command = entry["hooks"][0]["command"].as_str().unwrap();
            assert!(
                command.starts_with(&shell_quote(&adapter_path.display().to_string())),
                "every guard entry must be wrapped by the adapter: {command}"
            );
        }
    }

    /// design T-17 acceptance: `write_permission_config` must also render the
    /// permission-policy artifacts — a Boss-owned `sandbox.toml` custom
    /// profile and `extra_args` carrying `--sandbox` + the structural
    /// `--deny` rule set — not leave `extra_args` empty.
    #[tokio::test]
    async fn write_permission_config_renders_sandbox_and_deny_extra_args() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-t17-standard";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();

        let boss_data_dir = tmp.path().join("boss-data");
        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Standard,
            workspace_path: workspace.clone(),
            events_socket_path: boss_data_dir.join("events.sock"),
            boss_event_path: tmp.path().join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-1".into(),
            execution_kind: "task_implementation".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: None,
            checkleft_guard_script: None,
        };

        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();

        let grok_home = grok_home_for_run(run_id).unwrap();
        let sandbox_toml_path = grok_home.join("sandbox.toml");
        assert!(
            artifacts.config_files.contains(&sandbox_toml_path),
            "config_files must include sandbox.toml: {:?}",
            artifacts.config_files
        );
        let sandbox_toml = fs::read_to_string(&sandbox_toml_path).unwrap();
        assert!(sandbox_toml.contains("[profiles.boss-workspace]"), "{sandbox_toml}");
        assert!(sandbox_toml.contains("extends = \"workspace\""), "{sandbox_toml}");
        assert!(
            sandbox_toml.contains(&boss_data_dir.display().to_string()),
            "{sandbox_toml}"
        );

        assert_eq!(artifacts.extra_args[0], "--sandbox");
        assert_eq!(artifacts.extra_args[1], "boss-workspace");
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == "Bash(rm -rf:*)"),
            "must deny rm -rf: {:?}",
            artifacts.extra_args
        );
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == "Bash(sudo)"),
            "must deny sudo: {:?}",
            artifacts.extra_args
        );
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == "Bash(bossctl)"),
            "must deny bossctl: {:?}",
            artifacts.extra_args
        );
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == format!("Edit({})", boss_data_dir.display())),
            "must deny the Boss data dir: {:?}",
            artifacts.extra_args
        );
        // Standard worker: no forced --permission-mode (T-31 answer-agent
        // allowlist is a Grok follow-on, not yet built).
        assert!(!artifacts.extra_args.contains(&"--permission-mode".to_owned()));
    }

    /// Reviewer worker kind must select the read-only sandbox posture, both
    /// on the CLI (`--sandbox boss-read-only`) and in the custom profile's
    /// `extends`.
    #[tokio::test]
    async fn write_permission_config_reviewer_selects_read_only_sandbox() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-t17-reviewer";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Reviewer,
            workspace_path: workspace.clone(),
            events_socket_path: tmp.path().join("boss-data").join("events.sock"),
            boss_event_path: tmp.path().join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-1".into(),
            execution_kind: "pr_review".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: None,
            checkleft_guard_script: None,
        };

        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();
        assert_eq!(artifacts.extra_args[0], "--sandbox");
        assert_eq!(artifacts.extra_args[1], "boss-read-only");

        let grok_home = grok_home_for_run(run_id).unwrap();
        let sandbox_toml = fs::read_to_string(grok_home.join("sandbox.toml")).unwrap();
        assert!(sandbox_toml.contains("[profiles.boss-read-only]"), "{sandbox_toml}");
        assert!(sandbox_toml.contains("extends = \"read-only\""), "{sandbox_toml}");
    }

    /// Remote workers get no local `sandbox.toml` (no Boss data dir to fence)
    /// and the CLI `--sandbox` value must name a built-in profile, never the
    /// custom one that was never materialised (which would refuse the
    /// worker to start on an unresolvable `extends`).
    #[tokio::test]
    async fn write_permission_config_remote_omits_sandbox_toml_and_data_dir_deny() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-t17-remote";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Standard,
            workspace_path: workspace.clone(),
            events_socket_path: tmp.path().join("forwarded-events.sock"),
            boss_event_path: tmp.path().join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-1".into(),
            execution_kind: "task_implementation".into(),
            task_kind: None,
            is_remote: true,
            path_guard_script: None,
            checkleft_guard_script: None,
        };

        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();

        let grok_home = grok_home_for_run(run_id).unwrap();
        assert!(
            !artifacts.config_files.contains(&grok_home.join("sandbox.toml")),
            "remote must not write sandbox.toml: {:?}",
            artifacts.config_files
        );
        assert!(!grok_home.join("sandbox.toml").exists());
        assert_eq!(artifacts.extra_args[0], "--sandbox");
        assert_eq!(
            artifacts.extra_args[1], "workspace",
            "remote must use the built-in profile name"
        );
        assert!(
            !artifacts
                .extra_args
                .iter()
                .any(|a| a.starts_with("Read(") || a.starts_with("Edit(")),
            "remote must omit the Boss data dir deny pair: {:?}",
            artifacts.extra_args
        );
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == "Bash(bossctl)"),
            "structural non-path denies still apply on remote: {:?}",
            artifacts.extra_args
        );
    }

    /// Opt-in env var for the live permission-enforcement tests below.
    ///
    /// The grammar is unvalidated at *parse* time (`--deny '(((('` is
    /// accepted silently — investigation §Malformed / unknown rules), so
    /// this rule set needs a test proving it actually denies at *runtime*,
    /// not merely that it parses. That requires a real `grok -p` turn
    /// against xAI's API (billed, network-dependent) — unlike this file's
    /// other "live" tests, which only shell out to the free, local `grok
    /// inspect --json` / `--version`. Gating on grok-on-PATH alone would
    /// make routine `bazel test` on any dev machine with Grok installed
    /// silently spend real API budget, so this additionally requires an
    /// explicit opt-in env var on top of the usual soft-skip. Also requires
    /// `grok` to actually be reachable as a spawned subprocess (Bazel's test
    /// sandbox blocks this even when `grok --version` above passed via a
    /// wrapper — run the compiled `driver_test` binary directly, outside
    /// `bazel test`'s sandboxing, to exercise this for real.
    const LIVE_ENFORCEMENT_TEST_ENV: &str = "BOSS_GROK_LIVE_PERMISSION_ENFORCEMENT_TEST";

    fn live_enforcement_test_enabled() -> bool {
        if std::env::var(LIVE_ENFORCEMENT_TEST_ENV).as_deref() != Ok("1") {
            eprintln!(
                "{LIVE_ENFORCEMENT_TEST_ENV} not set to 1; skipping live permission-enforcement test \
                 (set it explicitly to run — this test makes real, billed grok-4.5 API calls)"
            );
            return false;
        }
        if std::process::Command::new("grok")
            .arg("--version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("grok not available; skipping live permission-enforcement test");
            return false;
        }
        if !dirs_home_grok_auth().exists() {
            eprintln!("no host ~/.grok/auth.json; skipping live permission-enforcement test");
            return false;
        }
        true
    }

    /// design T-17 acceptance (part 1/2): the `--sandbox boss-read-only`
    /// profile [`GrokDriver::write_permission_config`] selects for a
    /// `Reviewer` worker — backed by the real `sandbox.toml` this run writes
    /// — must kernel-deny a workspace write. Runs the *actual* driver
    /// output (hooks included, exactly as a real worker spawns), checked by
    /// real file-presence, not the model's self-report.
    #[tokio::test]
    async fn write_permission_config_live_sandbox_denies_workspace_write() {
        if !live_enforcement_test_enabled() {
            return;
        }

        // CWD must NOT be under /tmp or macOS's per-process TempDir
        // (`/var/folders/...`) — the investigation found every built-in
        // sandbox profile always grants write access there, which would make
        // this assertion vacuous (§"/tmp always writable — validation
        // hazard").
        let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for this test"));
        let scratch_root = real_home.join(".cache").join("boss-grok-permcfg-live-sandbox-test");
        let _cleanup = ScratchCleanup::new(&scratch_root);

        let homes = scratch_root.join("grok-homes");
        let workspace = scratch_root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes, &dirs_home_grok_auth(), true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-live-sandbox-1";
        driver
            .provision_workspace(&workspace, "live sandbox enforcement probe", run_id)
            .await
            .expect("provision must succeed");

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Reviewer,
            workspace_path: workspace.clone(),
            events_socket_path: scratch_root.join("boss-data").join("events.sock"),
            boss_event_path: scratch_root.join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-live".into(),
            execution_kind: "pr_review".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: None,
            checkleft_guard_script: None,
        };
        let artifacts = driver
            .write_permission_config(&input, scratch_root.as_path())
            .await
            .expect("write_permission_config must succeed");

        let grok_home = grok_home_for_run(run_id).unwrap();
        let process_home = process_home_for_run(run_id).unwrap();

        let probe_write = workspace.join("probe_write.txt");
        let _ = fs::remove_file(&probe_write);

        let output = run_grok_probe(
            &grok_home,
            &process_home,
            &workspace,
            "Try to create a file named probe_write.txt in the current directory with content \
             SHOULD_NOT_EXIST, using a write tool or shell. Report the outcome honestly, including any \
             denial/permission error text. Final line: LIVE_DONE",
            &artifacts.extra_args,
        );
        eprintln!(
            "live sandbox-write probe stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.status.success(),
            "live grok invocation failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            !probe_write.exists(),
            "workspace write must be denied by the boss-read-only sandbox profile: probe_write.txt was created"
        );
    }

    /// design T-17 acceptance (part 2/2): the structural `--deny
    /// 'Bash(rm -rf:*)'` / `'Bash(rm -rf *)'` rules must refuse `rm -rf` at
    /// the CLI permission-grammar layer, isolated from every other
    /// mechanism. Deliberately uses a hooks-free `GROK_HOME` and
    /// `--sandbox off`: this driver's real `PreToolUse` adapter fails closed
    /// on any invocation missing Grok's own hook-context env vars (a
    /// separate, legitimate guardrail — see `hooks.rs`), which otherwise
    /// denies every tool call and would make this check pass for the wrong
    /// reason. Isolating removes that confound so this test attributes the
    /// denial to the `--deny` rule set specifically, not to the hook layer.
    #[tokio::test]
    async fn write_permission_config_live_deny_rule_blocks_rm_rf_isolated_from_hooks() {
        if !live_enforcement_test_enabled() {
            return;
        }

        let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for this test"));
        let scratch_root = real_home.join(".cache").join("boss-grok-permcfg-live-deny-test");
        let _cleanup = ScratchCleanup::new(&scratch_root);

        let workspace = scratch_root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let grok_home = scratch_root.join("grok-home");
        fs::create_dir_all(&grok_home).unwrap();
        let process_home = scratch_root.join("process-home");
        fs::create_dir_all(&process_home).unwrap();

        // No hooks/, no sandbox.toml: only auth + base config + trust, so
        // the only lever in play is the `--deny` CLI flag under test.
        std::os::unix::fs::symlink(dirs_home_grok_auth(), grok_home.join("auth.json")).unwrap();
        fs::write(grok_home.join("config.toml"), render_base_config_toml()).unwrap();
        fs::write(
            grok_home.join("trusted_folders.toml"),
            home::render_trusted_folders_toml(&workspace),
        )
        .unwrap();

        let victim = workspace.join("victim.txt");
        fs::write(&victim, "victim\n").unwrap();

        // Only the rm -rf rules — `--sandbox off` so the sandbox cannot
        // contribute to (or confound) this result.
        let mut extra_args = vec!["--sandbox".to_owned(), "off".to_owned()];
        for rule in permissions::structural_deny_rules(None) {
            if rule.starts_with("Bash(rm") {
                extra_args.push("--deny".to_owned());
                extra_args.push(rule);
            }
        }

        let output = run_grok_probe(
            &grok_home,
            &process_home,
            &workspace,
            "Using a shell command, run exactly: rm -rf victim.txt . Report the outcome honestly, including \
             any denial/permission error text. Final line: LIVE_DONE",
            &extra_args,
        );
        eprintln!(
            "live deny-rule probe stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.status.success(),
            "live grok invocation failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            victim.exists(),
            "rm -rf must be denied by --deny 'Bash(rm -rf:*)'/'Bash(rm -rf *)': victim.txt was deleted"
        );
    }

    struct ScratchCleanup(PathBuf);
    impl ScratchCleanup {
        fn new(path: &Path) -> Self {
            let _ = fs::remove_dir_all(path);
            fs::create_dir_all(path).unwrap();
            Self(path.to_path_buf())
        }
    }
    impl Drop for ScratchCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_grok_probe(
        grok_home: &Path,
        process_home: &Path,
        workspace: &Path,
        prompt: &str,
        extra_args: &[String],
    ) -> std::process::Output {
        let session_id = home::new_session_uuid().unwrap();
        let mut cmd = std::process::Command::new("grok");
        cmd.env("GROK_HOME", grok_home)
            .env("HOME", process_home)
            .env_remove("GROK_FOLDER_TRUST")
            .arg("-p")
            .arg(prompt)
            .arg("--always-approve")
            .arg("--trust")
            .arg("--session-id")
            .arg(&session_id)
            .arg("--cwd")
            .arg(workspace)
            .arg("--output-format")
            .arg("json")
            .arg("--max-turns")
            .arg("6")
            .arg("--model")
            .arg("grok-4.5");
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.output().expect("grok -p invocation must spawn")
    }
}
