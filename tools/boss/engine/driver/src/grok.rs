//! `GrokDriver` — xAI Grok Build agent driver.
//!
//! Descriptor, capability set, model menu, Boss-owned `GROK_HOME` provisioning,
//! interactive-TUI pane spawn, hook wiring (progress forwarder + the five
//! `PreToolUse` guards behind a canonicalisation adapter — design T-09, see
//! [`hooks`]), the four ControlVerbs and Grok/xAI error classification
//! (design T-13, see [`classify_error`] and this file's `probe`/`interrupt`/
//! `stop`/`reap` overrides), and turn-end recovery for Esc-cancelled turns
//! (design T-12, see [`turn_end_recovery`]). `TranscriptAccess` (design T-11)
//! is implemented via [`transcript`]: `transcript_path_for_session` reads
//! Grok's stamped `transcriptPath` and rewrites it onto Boss's durable
//! transcript store (the per-run `$GROK_HOME/sessions` link is reclaimed),
//! and [`GrokTranscriptSession`] normalises the ACP `sessionUpdate` dialect.
//! Full permission-policy artifacts
//! (`--sandbox`/`--allow`/`--deny`, T-17) remain follow-on work — see
//! `tools/boss/docs/designs/grok-as-a-first-class-interactive-agent-driver.md`.
//!
//! Behavioural fan-out lives under the `grok/` submodule directory so
//! concurrent follow-on tasks (output capture, characterisation) do not
//! serialise on this file.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{NormalizeError, PaneMonitorSpec, WorkerEvent};
use boss_ssh_transport::shell_quote;
use serde_json::Value;

use crate::transcript_store::{
    durable_sessions_dir, persistable_transcript_path, transcript_store_root, verified_durable_sessions_dir,
};

/// Namespace this driver's durable transcripts are filed under in Boss's
/// worker transcript store. Every caller that resolves, provisions, or grants
/// access to that directory must agree on it, so it lives here rather than as
/// a repeated literal.
pub(crate) const TRANSCRIPT_DRIVER_SLUG: &str = "grok";

mod classify_error;
mod environment;
mod home;
mod hooks;
mod model_menu;
mod permissions;
mod preflight;
mod progress;
mod transcript;
mod turn_end_recovery;

pub(crate) use home::GROK_HOMES_DIR_NAME;
pub use home::{
    COMPAT_SURFACES, COMPAT_VENDORS, GROK_AUTH_SOURCE_ENV, GROK_HOMES_ENV_TEST_LOCK, GROK_HOMES_ROOT_ENV,
    GROK_SKIP_POSTURE_ASSERT_ENV, GrokRuntimeState, assert_grok_home_safe_to_delete, assert_inspect_json_posture,
    grok_home_for_run, grok_homes_root, process_home_for_run, reclaim_grok_home, render_base_config_toml,
    resolve_grok_auth_source, trust_path_variants,
};

use classify_error::classify_grok_error;
use environment::GrokProcessEnvironment;
use home::{provision_grok_home, read_session_id, read_workspace_path_stamp};
use progress::GrokProgressSession;
use transcript::GrokTranscriptSession;
use turn_end_recovery::{is_cancelled_turn_end, prepare_snapshot};

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, HookWiringDestination,
    InterruptDelivery, InterruptGesture, InterruptPlan, InterruptRecoverySnapshot, ModelMenu, PermissionArtifacts,
    PermissionInput, PrUrlCaptureFeed, ProbeDelivery, ProgressFidelity, ProgressIngress, ProgressObservationConfig,
    ProgressObservationWiring, ProgressSessionNormalizer, ReapDelivery, SpawnPlan, SpawnRequest, StopDelivery,
    StructuredOutputArtifacts, StructuredOutputRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring,
    TranscriptSessionNormalizer, TurnEnd, TurnEndEvidence, WorkerErrorClass, WorkerKind,
    default_structured_output_wiring,
};

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------
//
// Model menu refresh path: `grok models` is the machine-readable source.
// The current-default table below must be refreshed whenever xAI changes the
// catalog — update engine_default / model_for_reasoning /
// default_model_for_level together. Do not hard-freeze forever, and never
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
        // Authenticated `grok models` reported this as the default on
        // 2026-08-18. Step-5 fall-through and every classified row resolve
        // to the current generation.
        engine_default: "grok-4.6",
        effort_value_for_level: model_menu::effort_value_for_level,
        default_model_for_level: model_menu::default_model_for_level,
        model_for_reasoning: model_menu::model_for_reasoning,
        review_model_for_tier: model_menu::review_model_for_tier,
        design_investigation_model: None,
        prompt_addendum_for_level: model_menu::prompt_addendum_for_level,
        model_requires_auto_permissions: model_menu::model_requires_auto_permissions,
        model_belongs_to_driver: model_menu::model_belongs_to_driver,
    },
};

/// Preamble for the agent-rules file (`AGENTS.md`). Names Grok observability
/// rather than Claude hooks so the shared body below it is not lying about
/// the mechanism this session uses.
///
/// Also carries a Grok-specific paragraph raising the bar at which this
/// worker stops to ask a question rather than proceeding on a stated
/// assumption; Grok defaults to asking far more readily than the other
/// drivers on equivalent ambiguity. It lives in this driver-scoped preamble
/// (see [`crate::AgentDriver::agent_rules_preamble`]) rather than in
/// `model_menu::prompt_addendum_for_level`, which is gated by `EffortLevel`
/// and returns `None` for Trivial/Small rows, so it reaches every Grok
/// worker. The shared body is unchanged, so Claude and Codex workers see no
/// difference.
///
/// A second Grok-specific paragraph forbids entering an interactive
/// approval gate (plan-approval mode being the observed case: a worker on
/// an ordinary chore opened a `plan.md` review UI and sat idle waiting for
/// a keypress that never came). That is a distinct failure from asking a
/// question the ask-threshold paragraph above already covers — the worker
/// isn't asking anything answerable by a default, it's entered a UI whose
/// only exit is a human pressing a key. Planning and recording a plan are
/// still fine; blocking on approval of one is not.
const GROK_AGENT_RULES_PREAMBLE: &str = "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via Grok hooks under a Boss-owned GROK_HOME.\n\
     For ordinary pre-push validation, run `bin/checkleft run` with no flags; use\n\
     `checkleft --all` only in CI, when modifying checkleft itself, or with a\n\
     strong stated justification.\n\
     \n\
     Default to proceeding on a reasonable assumption instead of stopping to\n\
     ask the human: pick the interpretation a competent engineer would\n\
     default to, implement it, and record the assumption explicitly (in your\n\
     PR body or final summary) so it is visible to whoever reviews the work.\n\
     Reserve asking for cases where proceeding either way would be unsafe,\n\
     destructive, or irreversible (e.g. data loss, an unrecoverable\n\
     production action, a missing credential), or where a wrong guess would\n\
     waste substantial work (e.g. a fork the rest of the task's shape\n\
     depends on). Ordinary ambiguity that a reasonable default resolves is\n\
     not, by itself, a reason to stop.\n\
     \n\
     Never enter an interactive approval gate that halts the run waiting on\n\
     a human keypress: plan-approval mode, a \"waiting for approval\" state,\n\
     a confirmation prompt you yourself initiate, or any other interactive\n\
     affordance whose exit condition is a human acting on it. You run\n\
     autonomously with nobody watching the pane to press a key. You may\n\
     still reason about approach and write a plan; what you must not do is\n\
     block on approval of it. If you want a human to see your plan, put it\n\
     in the PR body once the work is done — that PR is your output\n\
     surface, not an interactive prompt mid-run. This does not change the\n\
     genuinely blocking cases above (unsafe, destructive, or irreversible\n\
     actions, or a missing credential) — those remain reasons to stop\n\
     and emit a blocked marker, not reasons to open an approval UI.";

// ---------------------------------------------------------------------------
// GrokDriver
// ---------------------------------------------------------------------------

/// xAI Grok Build CLI driver.
///
/// Registered under the `"grok"` slug. Declares the v1 capability set from
/// the Grok driver design. Workspace provisioning, pane spawn, progress
/// wiring, and the ControlVerbs surface (probe/interrupt/stop/reap,
/// classify_error, turn-end recovery) are live; full permission artifacts
/// (`--sandbox`/`--allow`/`--deny`) remain follow-on work.
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
        debug_assert!(
            matches!(*e, "low" | "medium" | "high"),
            "grok rejects effort {e:?} at request time; only low|medium|high are accepted",
        );
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
    // Kept because of a MEASURED progress-attribution defect, not merely as
    // v1 posture. Probed against `grok 1.0.0` in this exact pane shape —
    // `docs/investigations/grok-subagent-hook-attribution-2026-08-09.md`:
    //
    // - A subagent DOES inherit the global `$GROK_HOME/hooks/` set, and its
    //   tool calls ARE intercepted: a `PreToolUse` `deny` blocked a
    //   subagent's shell call exactly as it blocks the top-level session's.
    //   There is no safety gap, and that is not why this flag is here.
    // - It also fires `session_end` at its own turn end, with a payload
    //   whose key set and `reason` ("shutdown") are IDENTICAL to the
    //   top-level session's — only the `sessionId` value differs. Boss
    //   routes hook events by `_boss_run_id` (`events_socket.rs:340-370`),
    //   which is the same for both, and `live_worker_state.rs:1022` applies
    //   `SessionEnd` by slot, so a finishing subagent flips a live worker to
    //   `WorkerActivity::Terminated` and publishes `AnswerAgentDied`. For a
    //   background subagent, the slot sits in `Terminated` from the child's
    //   `session_end` until the parent's next event — about seven seconds in
    //   the measured run — refusing nudges, interrupts, and answer delivery
    //   while `activity_for_run` reports no live worker.
    // - `background_children.rs` cannot compensate: a Grok subagent is
    //   in-process (every hook forks from the same `grok` pid), so
    //   `count_live_descendants` reads 0 across the subagent's think/model
    //   windows. `Stop.backgroundTasks` — the documented alternative — was
    //   empirically `[]` with a background subagent already in flight.
    //
    // Removing this flag needs session-identity filtering at ingress and a
    // tracked `SubagentStart`/`SubagentStop` pair first; see the
    // investigation's "What would have to change" section.
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
        // Provided (all except ToolProvisioning + AwaitingInputSignal +
        // CommandOutcomeObservation):
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
            // Synthesize). The vocabulary is now *measured*, and the
            // omission is the measured result rather than caution:
            // `grok-notification-vocabulary-and-leader-process-2026-07-29.md`.
            //
            // A genuine awaiting-input signal exists —
            // `notificationType: "permission_prompt"` (level `info`,
            // "Tool permission requested") — but it is raised only by an
            // interactive permission prompt, and Boss spawns with
            // `--always-approve`, which suppresses that prompt. Under Boss's
            // own flags the only `Notification` observed on the wire was
            // `task_complete` ("Background task completed: <id>"), which
            // means the opposite of blocked. Neither a `PreToolUse` hook
            // deny nor a `--deny` rule raises one, and `--always-approve`
            // also suppresses the folder-trust dialog outright.
            //
            // So there is nothing honest to bind to: mapping this capability
            // onto `task_complete` would be exactly the fabricated
            // WaitingForInput the contract prohibits. A Grok worker shows
            // Working/Idle and never a fabricated WaitingForInput
            // (design G-13 / T-24). This becomes earnable only if Boss ever
            // spawns Grok workers *without* `--always-approve`, at which
            // point `permission_prompt` is the already-measured mapping.
            //
            // CommandOutcomeObservation — omitted → default Degrade (never
            // Synthesize). Grok's stdout stream has not been characterised
            // for a reliable per-command exit-status field the way Codex's
            // rollout `exit_code`/`status` fields were investigated and
            // found unreliable; absent that evidence, this stays
            // undeclared rather than assumed.
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
            // --always-approve. "Grok 4" matches footer "Grok 4.6 …"
            // without pinning the patch model id — the prefix is
            // deliberately generation-agnostic, so it keeps matching
            // across future SKU bumps too.
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
        let auth_source = resolve_grok_auth_source();

        let environment = match GrokProcessEnvironment::resolve(&grok_home, &process_home, &auth_source) {
            Ok(environment) => environment,
            Err(error) => {
                tracing::error!(run_id, error = %error, "refusing to spawn Grok worker with unresolved host tool environment");
                return SpawnPlan {
                    env: Vec::new(),
                    command: format!(
                        "echo {} >&2; exit 1\n",
                        shell_quote(&format!("Grok worker environment resolution failed: {error:#}"))
                    ),
                };
            }
        };

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
            // `environment` preserves the T-01 HOME quarantine while
            // delegating the host Cube/gh/jj/git stores and the one shared
            // Grok OAuth path. Provisioning applied this exact same contract
            // to the fail-fast capability checks before we reached spawn.
            env: environment.directives(),
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
        // payload must surface loudly. The full transcript is already in the
        // durable sessions link; this temporary home remains reclaimable.
        assert_grok_home_safe_to_delete(&runtime.grok_home)?;
        Ok(())
    }

    /// Write the `$GROK_HOME` hook wiring (design T-09): the `boss-event`
    /// progress forwarder on every lifecycle event, plus the five
    /// `PreToolUse` guards behind [`hooks::write_hooks`]'s canonicalisation
    /// adapter. Overwrites the provisional canary `provision_workspace`
    /// wrote.
    ///
    /// Also writes the full permission-policy artifacts. Local macOS workers
    /// use `--sandbox off`: their terminal commands are not OS-confined, so
    /// repository test actions can install their own hermetic Seatbelt profile.
    /// Other platforms retain Grok's custom `sandbox.toml`. CLI `--deny` rules
    /// remain an independent belt for `rm -rf`, `sudo`, and `bossctl`.
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
            is_reviewer: input.worker_kind == WorkerKind::Reviewer,
            run_id: Some(input.run_id.clone()),
            workspace_path: Some(input.workspace_path.clone()),
        };

        let mut config_files = hooks::write_hooks(&grok_home, &obs_config, &interception)
            .with_context(|| format!("writing Grok hook wiring under {}", grok_home.display()))?;
        config_files.push(grok_home.join("config.toml"));

        // Boss data dir for the CLI Read/Edit belt — `None` on remote (see
        // `interception.data_dir` above).
        let boss_data_dir = if input.is_remote {
            None
        } else {
            input.events_socket_path.parent().map(|p| p.to_path_buf())
        };

        if !permissions::grok_sandbox_disabled(input.is_remote) && !input.is_remote {
            let sandbox_toml_path = grok_home.join("sandbox.toml");
            fs::write(
                &sandbox_toml_path,
                permissions::render_sandbox_toml(input.worker_kind, boss_data_dir.as_deref()),
            )
            .with_context(|| format!("writing {}", sandbox_toml_path.display()))?;
            config_files.push(sandbox_toml_path);
        }

        let extra_args = permissions::extra_args(
            input.worker_kind,
            boss_data_dir.as_deref(),
            &input.workspace_path,
            input.is_remote,
        );
        let sandbox_profile = extra_args
            .get(1)
            .context("Grok permission rendering omitted the required --sandbox profile")?;
        permissions::ensure_build_tool_capability(input.is_remote, sandbox_profile)?;

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

    /// Design T-19. The inherited [`super::default_pr_url_capture_feed`]
    /// scans a Claude-shaped `tool_response.{stdout,stderr}` object; Grok's
    /// canonicalised Bash-shaped `tool_response` (the `toolResult` rename —
    /// see [`progress`]) carries neither key. Verified against a real
    /// capture (`grok-pretooluse-decision-vocabulary-artifacts/hook_payloads/
    /// PostToolUse.run_terminal_command.sample.json` under
    /// `tools/boss/docs/investigations/`):
    /// ```json
    /// { "type": "Bash", "output": [104, 105, 10], "output_for_prompt": "exit: 0\nhi\n",
    ///   "exit_code": 0, "command": "…", "truncated": false, … }
    /// ```
    /// Unadapted, the default's object arm hits its "neither `stdout` nor
    /// `stderr` present" early-return and comes back `None` for every single
    /// Grok Bash call — the primary path silently never fires and every PR
    /// falls onto the reconstruction fallback, exactly the failure state
    /// design T-19 forbids. Read `output_for_prompt` instead — the
    /// exit-annotated combined-output text observed on the wire — falling
    /// back to a lossy decode of the raw `output` byte array so a future
    /// toolResult shape that drops `output_for_prompt` still yields
    /// something scannable rather than going dark again.
    fn pr_url_capture_feed(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        tool_response: &serde_json::Value,
    ) -> Option<PrUrlCaptureFeed> {
        if tool_name != "Bash" {
            return None;
        }
        let command = crate::command_from_tool_input(tool_input);
        Some(PrUrlCaptureFeed {
            output_text: grok_bash_output_text(tool_response),
            command,
        })
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

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        // Grok stamps the absolute path to the session's ACP update stream
        // on every hook payload it emits — `transcriptPath`, pointing at
        // `$GROK_HOME/sessions/<pct-encoded-cwd>/<sid>/updates.jsonl`
        // (design §"Session, turn, and transcript identity"). Same key
        // rename ClaudeDriver's own field read is, per that section — no
        // glob, no derivation, no directory watch. Empty strings are
        // treated as missing, mirroring ClaudeDriver::transcript_path_for_session.
        //
        // `$GROK_HOME/sessions` is a symlink into Boss's durable store.
        // Record the durable target: the per-run home is reclaimed, and a
        // pointer through that link dies with it.
        let s = raw.get("transcriptPath")?.as_str()?;
        if s.is_empty() {
            None
        } else {
            Some(persistable_transcript_path(Path::new(s)).to_string_lossy().into_owned())
        }
    }

    fn transcript_session(&self) -> Option<Box<dyn TranscriptSessionNormalizer>> {
        // `tool_call` and `tool_call_update` arrive as separate ACP
        // records, joined only by `toolCallId` — GrokTranscriptSession
        // owns that per-tail correlation (see `grok/transcript.rs`).
        Some(Box::new(GrokTranscriptSession::default()))
    }

    fn transcript_containment_root(&self, run_id: &str) -> anyhow::Result<Option<PathBuf>> {
        let grok_home = grok_home_for_run(run_id)?;
        let sessions_path = grok_home.join("sessions");
        match fs::symlink_metadata(&sessions_path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let store_root = transcript_store_root()?;
                return Ok(Some(verified_durable_sessions_dir(
                    &grok_home,
                    &store_root,
                    TRANSCRIPT_DRIVER_SLUG,
                    run_id,
                )?));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let store_root = transcript_store_root()?;
                let durable = durable_sessions_dir(&store_root, TRANSCRIPT_DRIVER_SLUG, run_id)?;
                if let Ok(canonical) = fs::canonicalize(&durable)
                    && canonical.is_dir()
                {
                    return Ok(Some(canonical));
                }
            }
            Err(err) => tracing::debug!(
                ?err,
                path = %sessions_path.display(),
                "stat of temporary sessions path failed; falling back to legacy containment"
            ),
            _ => {}
        }
        Ok(None)
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        // Isolated single-record reshape for direct callers that do not
        // hold a `transcript_session()` tail. Each call starts a fresh,
        // empty correlation state, so an isolated `tool_call_update` never
        // finds its matching `tool_call` and normalises to a bare system
        // filler — the live-status tail instead calls `transcript_session()`
        // so the pairing survives across the whole tail.
        transcript::normalize_acp_update(raw)
    }

    fn extract_error_from_transcript(&self, _lines: &[serde_json::Value]) -> Option<String> {
        None
    }

    fn classify_error(&self, raw_output: &str) -> WorkerErrorClass {
        // xAI/Grok-specific classification (design T-13, `grok/classify_error.rs`).
        // Must not route through `classify_claude_error` — Grok's own
        // `StopFailure` vocabulary only partially overlaps with Claude's.
        classify_grok_error(raw_output)
    }

    /// Probe is typed pane input (`SendToPane`) — Grok's interactive TUI
    /// reads stdin as the next user message, same as Claude's.
    fn probe(&self) -> ProbeDelivery {
        ProbeDelivery::PaneText
    }

    /// Interrupt is Esc into the pane (`InterruptWorkerPane`) — verified by
    /// the Q8 spike to cancel the in-flight turn while the process survives
    /// and accepts a subsequent turn. Esc-cancelled turns skip the `Stop`
    /// hook entirely (design G-7); [`Self::prepare_interrupt_recovery`] /
    /// [`Self::is_interrupt_recovery_turn_end`] close that gap (design T-12).
    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::PaneEsc
    }

    /// One Escape, confirmed through the interrupt-recovery observer rather
    /// than a `Stop` hook.
    ///
    /// Measured in `docs/investigations/ghostty-grok-pane-viability.md` Q8
    /// (`SPIKE_SCENARIO=esc_interrupt`, session telemetry committed under
    /// `ghosttykit_host/evidence/esc_interrupt/`): a single Esc at ~6s into a
    /// `sleep 45` tool call cancelled the turn with
    /// `cancellation_context.trigger = "esc"`, the process survived, and the
    /// immediately following probe landed (`redirect_kind:
    /// cancel_then_send`). That is exactly the gesture this plan encodes.
    ///
    /// The evidence is [`TurnEndEvidence::RecoveryObserver`] because a
    /// Grok turn cancelled this way **skips the `Stop` hook entirely** — the
    /// only record is `turn_ended`/`cancelled` in `events.jsonl`, which is
    /// what [`Self::prepare_interrupt_recovery`] /
    /// [`Self::is_interrupt_recovery_turn_end`] exist to read. The confirm
    /// window matches that observer's own
    /// [`crate::grok::turn_end_recovery::SETTLE_WINDOW`], so an attempt is
    /// never declared failed while the observer it depends on is still
    /// legitimately waiting.
    ///
    /// **Declared limitation:** Esc does not cancel when Grok's TUI is in
    /// fullscreen vim mode (xAI's own docs; Ctrl+C is the gesture there).
    /// Boss never enables vim mode for workers — [`crate::grok::home`] writes
    /// the worker's config with `vim_mode` off precisely so this plan holds —
    /// but if a worker ever ends up in that mode the interrupt will exhaust
    /// its attempts and fail loudly rather than silently typing into a
    /// running turn.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        Some(InterruptPlan {
            gesture: InterruptGesture {
                key: "Escape",
                presses: 1,
                press_interval: Duration::from_millis(120),
            },
            confirm_window: turn_end_recovery::SETTLE_WINDOW,
            max_attempts: 2,
            turn_end_evidence: TurnEndEvidence::RecoveryObserver,
        })
    }

    /// Stop is `/quit` typed into the pane, then pane release — Grok has no
    /// documented signal-only graceful-quit path, but its interactive TUI
    /// accepts `/quit` as a full line and exits cleanly (Q8 spike).
    fn stop(&self) -> StopDelivery {
        StopDelivery::PaneCommand { command: "/quit" }
    }

    /// Reap is the universal SIGTERM→SIGKILL process-group ladder, which
    /// already covers tool child shells `run_terminal_command` spawns under
    /// the worker's process group — no Grok-specific reap behaviour is
    /// needed beyond the trait default.
    fn reap(&self) -> ReapDelivery {
        ReapDelivery::ProcessGroup
    }

    /// Pre-interrupt snapshot for T-12's turn-end recovery: resolves
    /// `GROK_HOME`, the stamped session id and workspace path, and the
    /// current length of `events.jsonl` (see `grok/turn_end_recovery.rs`).
    /// `None` when this run has no resolvable per-run state (never
    /// provisioned, or already torn down) — interrupt delivery itself is
    /// unaffected either way.
    fn prepare_interrupt_recovery(&self, run_id: &str) -> Option<InterruptRecoverySnapshot> {
        prepare_snapshot(run_id)
    }

    /// Recognise a cancelled-turn-end `events.jsonl` record — see
    /// `grok/turn_end_recovery.rs` for the exact match rule and its
    /// empirical grounding.
    fn is_interrupt_recovery_turn_end(&self, raw: &serde_json::Value) -> bool {
        is_cancelled_turn_end(raw)
    }

    // mid_turn_pane_input: intentionally NOT overridden. Trait default is
    // Rejects — the safe answer until mid-turn stdin consumption is proven
    // empirically for the interactive TUI (design G-10 / T-13). Structural
    // arguments that Grok "should" Buffer are not enough to flip the
    // default.

    /// Design T-18. The driver-neutral `BOSS_STRUCTURED_OUTPUT` /
    /// `BOSS_PR_URL_OUTPUT` env-file contract already applies to every
    /// worker — including Grok — unconditionally, from
    /// `core/src/runner/prompt.rs::structured_output_env_vars`, which calls
    /// [`default_structured_output_wiring`] directly rather than through
    /// this trait method. So the file contract needs no Grok-specific code;
    /// this override exists to record the evaluation of the native
    /// alternative rather than to change behaviour.
    ///
    /// `--json-schema <SCHEMA>` ("implies `--output-format json`") was
    /// evaluated and is **not** adopted. Evidence: `grok --help` documents
    /// `--output-format` itself as "Output format for headless mode"
    /// (default `plain`) — a headless-only concept. Probed live against a
    /// real pty (`grok 0.2.112`, `--json-schema <file> --no-alt-screen
    /// --always-approve --trust --cwd … --session-id … "<prompt>"`,
    /// isolated `GROK_HOME`): the flag combination parses and the TUI
    /// starts and renders normally — it is not rejected at the CLI-parse
    /// level. But nothing available to this driver-wiring task can drive a
    /// full turn to completion and read the rendered pane text to confirm
    /// the flag constrains output *there* rather than silently no-oping
    /// (or, worse, coercing the session toward a one-shot JSON-envelope
    /// render that would break the pane worker shape) — that needs the
    /// GhosttyKit AppKit harness the pane-viability spike used
    /// (`tools/boss/docs/investigations/ghostty-grok-pane-viability.md`
    /// Appendix A), out of scope here. Per T-18: "if it does not work,
    /// record the negative result... so a later pass does not
    /// re-investigate" — recorded. The env-file contract remains the sole
    /// `StructuredOutput` mechanism for Grok; do not wire `--json-schema`
    /// on the strength of it merely parsing.
    fn structured_output_wiring(
        &self,
        request: &StructuredOutputRequest<'_>,
    ) -> anyhow::Result<StructuredOutputArtifacts> {
        Ok(default_structured_output_wiring(request))
    }

    fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
        // No Grok-specific prose-scrape conventions yet. Empty Vec: primary
        // channel is the file contract (default structured_output_wiring).
        Vec::new()
    }
}

/// Extract the free text [`AgentDriver::pr_url_capture_feed`] feeds to the
/// shared PR-URL regex from a Grok Bash-shaped `tool_response` (post
/// `toolResult` → `tool_response` rename; see [`progress`]).
///
/// Prefers `output_for_prompt` — the `exit: N\n`-prefixed combined-output
/// string observed on the wire, the same text Grok's own prompt rendering
/// uses, so a PR URL printed by `gh pr create` / `cube pr create` lands in
/// it exactly as it would in a Claude `stdout` field. A present-but-empty
/// `output_for_prompt` is treated as absent so a genuinely empty capture
/// still tries the byte fallback rather than short-circuiting on `""`.
/// Falls back to a lossy UTF-8 decode of the raw `output` byte array on the
/// chance a future toolResult shape omits `output_for_prompt`, so this never
/// regresses to silently scanning nothing. A bare-string `tool_response` is
/// handled first, mirroring [`crate::default_pr_url_capture_feed`]'s own
/// bare-string arm, so a future Grok ingress that ever delivers free text
/// instead of an object still degrades gracefully instead of going dark.
fn grok_bash_output_text(tool_response: &Value) -> String {
    if let Some(text) = tool_response.as_str() {
        // Degrade the same way the default does if a future Grok ingress
        // ever delivers tool_response as free text instead of an object.
        return text.to_owned();
    }
    if let Some(text) = tool_response
        .get("output_for_prompt")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        return text.to_owned();
    }
    let Some(bytes) = tool_response.get("output").and_then(Value::as_array) else {
        return String::new();
    };
    let bytes: Vec<u8> = bytes.iter().filter_map(Value::as_u64).map(|n| n as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvDirective;
    use crate::test_support::home_override;
    use crate::{AbsenceDisposition, Capability, MidTurnPaneInput};
    use boss_protocol::{EffortLevel, ReasoningMode, ReviewModelTier};
    use serde_json::json;
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
        // Present unless `HOME` is already guarded by `HomeOverride`.
        // In either case, the environment remains serialised for this guard's
        // lifetime.
        _lock: Option<std::sync::MutexGuard<'static, ()>>,
        _transcript_store: crate::test_support::TranscriptStoreOverride,
        prior_homes: Option<std::ffi::OsString>,
        prior_auth: Option<std::ffi::OsString>,
        prior_skip: Option<std::ffi::OsString>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: either this guard owns ENV_LOCK or the HomeOverride
            // declared before it still owns that same lock.
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
        env_for_provision_with_lock(homes, auth_src, skip_inspect, Some(lock))
    }

    /// Configure provision-specific variables while `HOME` is guarded by
    /// [`home_override`], which already owns `ENV_LOCK`.
    fn env_for_provision_with_home_override(homes: &Path, auth_src: &Path, skip_inspect: bool) -> EnvGuard {
        env_for_provision_with_lock(homes, auth_src, skip_inspect, None)
    }

    fn env_for_provision_with_lock(
        homes: &Path,
        auth_src: &Path,
        skip_inspect: bool,
        lock: Option<std::sync::MutexGuard<'static, ()>>,
    ) -> EnvGuard {
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
            _transcript_store: crate::test_support::transcript_store_override(&homes.join("transcripts")),
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

    /// The ask-threshold guidance lands in Grok's agent-rules preamble
    /// (rendered once per session, ahead of the shared body every driver
    /// gets) rather than the effort-level-gated `prompt_addendum`, so it
    /// reaches every Grok worker regardless of the row's classified effort.
    #[test]
    fn agent_rules_preamble_raises_the_ask_threshold() {
        let preamble = GrokDriver::default().agent_rules_preamble();
        assert!(
            preamble.contains("Default to proceeding on a reasonable assumption"),
            "preamble must guide Grok workers to proceed on a stated assumption: {preamble}",
        );
        assert!(
            preamble.contains("Reserve asking for cases"),
            "preamble must still reserve asking for genuinely blocking cases: {preamble}",
        );
        // The pre-existing observability paragraph must survive untouched.
        assert!(preamble.contains("via Grok hooks under a Boss-owned GROK_HOME"));
    }

    /// Extends the ask-threshold guidance to cover interactive approval
    /// gates (plan-approval mode being the observed case): a Grok worker
    /// that never asks a question can still halt a run by opening a UI
    /// that only exits on a human keypress. Both paragraphs must survive
    /// together in the same driver-scoped preamble.
    #[test]
    fn agent_rules_preamble_forbids_interactive_approval_gates() {
        let preamble = GrokDriver::default().agent_rules_preamble();
        assert!(
            preamble.contains("Never enter an interactive approval gate"),
            "preamble must forbid blocking on plan-approval / confirmation UIs: {preamble}",
        );
        assert!(
            preamble.contains("plan-approval mode"),
            "preamble must name plan-approval mode as a covered case: {preamble}",
        );
        assert!(
            preamble.contains("put it\nin the PR body once the work is done"),
            "preamble must redirect plans to the PR body instead of an approval prompt: {preamble}",
        );
        // The ask-threshold paragraph must still be present alongside it.
        assert!(preamble.contains("Default to proceeding on a reasonable assumption"));
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

    // ── pr_url_capture_feed (design T-19) ───────────────────────────────

    /// Real `toolResult` shape captured by the T-02 investigation
    /// (`grok-pretooluse-decision-vocabulary-artifacts/hook_payloads/PostToolUse.run_terminal_command.sample.json`),
    /// minus the fields this module never reads. `output` decodes (as
    /// bytes) to `"/opt/homebrew/bin/git\n/opt/homebrew/bin/jj\n/opt/homebrew/bin/gh\n"`.
    fn real_bash_tool_result() -> Value {
        json!({
            "type": "Bash",
            "output": [
                47, 111, 112, 116, 47, 104, 111, 109, 101, 98, 114, 101, 119, 47, 98, 105, 110, 47, 103,
                105, 116, 10, 47, 111, 112, 116, 47, 104, 111, 109, 101, 98, 114, 101, 119, 47, 98, 105,
                110, 47, 106, 106, 10, 47, 111, 112, 116, 47, 104, 111, 109, 101, 98, 114, 101, 119, 47, 98,
                105, 110, 47, 103, 104, 10
            ],
            "output_for_prompt": "exit: 0\n/opt/homebrew/bin/git\n/opt/homebrew/bin/jj\n/opt/homebrew/bin/gh\n",
            "exit_code": 0,
            "command": "echo SHELL_OK > toolmap_shell.txt && which git jj gh || true",
            "truncated": false,
            "signal": Value::Null,
            "timed_out": false,
        })
    }

    #[test]
    fn grok_bash_output_text_prefers_output_for_prompt() {
        assert_eq!(
            grok_bash_output_text(&real_bash_tool_result()),
            "exit: 0\n/opt/homebrew/bin/git\n/opt/homebrew/bin/jj\n/opt/homebrew/bin/gh\n",
        );
    }

    #[test]
    fn grok_bash_output_text_falls_back_to_decoding_raw_output_bytes() {
        let tool_result = json!({
            "type": "Bash",
            "output": [104, 105, 10], // "hi\n"
            "exit_code": 0,
        });
        assert_eq!(grok_bash_output_text(&tool_result), "hi\n");
    }

    #[test]
    fn grok_bash_output_text_is_empty_when_neither_field_present() {
        assert_eq!(grok_bash_output_text(&json!({"type": "Bash"})), "");
    }

    #[test]
    fn pr_url_capture_feed_reads_output_for_prompt_not_stdout_stderr() {
        // Regression for the T-19 gap: the inherited default scans
        // tool_response.{stdout,stderr}, which Grok's real Bash toolResult
        // shape simply does not have (see real_bash_tool_result above). This
        // must not silently come back None.
        let tool_input = json!({"command": "which git jj gh"});
        let feed = GrokDriver::default()
            .pr_url_capture_feed("Bash", &tool_input, &real_bash_tool_result())
            .expect("Grok Bash observation must yield a feed");
        assert_eq!(feed.command, "which git jj gh");
        assert!(feed.output_text.contains("/opt/homebrew/bin/git"));
    }

    #[test]
    fn pr_url_capture_feed_extracts_real_pr_url_from_output_for_prompt() {
        let tool_input = json!({"command": "cube pr create --branch boss/exec_x --title t"});
        let tool_response = json!({
            "type": "Bash",
            "output_for_prompt": "exit: 0\nhttps://github.com/spinyfin/mono/pull/458\n",
            "exit_code": 0,
        });
        let feed = GrokDriver::default()
            .pr_url_capture_feed("Bash", &tool_input, &tool_response)
            .expect("feed");
        // The shared regex + command gates live in `engine/core`, one-way
        // downstream of this crate (`pr_url_capture.rs`); this test only
        // proves the driver hands it the right text. Use the shared regex
        // crate directly (already a dependency here) rather than
        // reimplementing the match.
        let url = boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).expect("url");
        assert_eq!(url, "https://github.com/spinyfin/mono/pull/458");
        assert_eq!(feed.command, "cube pr create --branch boss/exec_x --title t");
    }

    #[test]
    fn pr_url_capture_feed_end_to_end_from_raw_hook_payload() {
        // Regression for the T-19 acceptance criterion end to end: wire
        // payload (toolName "run_terminal_command") -> normalize_progress_event
        // -> pr_url_capture_feed -> find_first_pr_url. The other
        // pr_url_capture_feed tests hand-build already-canonicalised "Bash"
        // inputs, and post_tool_use_run_terminal_command_preserves_bash_shaped_tool_result_verbatim
        // (grok/progress.rs) covers the toolName -> "Bash" rename alone —
        // neither exercises the full chain the acceptance criterion depends
        // on, so this test fails if either half regresses.
        let raw = json!({
            "hookEventName": "post_tool_use",
            "sessionId": "0c4c0914-5e64-432c-90fa-dcdad9ff5957",
            "toolName": "run_terminal_command",
            "toolInput": {"command": "cube pr create --branch boss/exec_x --title t"},
            "toolResult": {
                "type": "Bash",
                "output": [],
                "output_for_prompt": "exit: 0\nhttps://github.com/spinyfin/mono/pull/458\n",
                "exit_code": 0,
                "truncated": false,
            },
        });

        let event = GrokProgressSession::new()
            .normalize_progress_event(&raw)
            .expect("normalizes");
        let WorkerEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
            ..
        } = event
        else {
            panic!("expected PostToolUse, got {event:?}");
        };

        let feed = GrokDriver::default()
            .pr_url_capture_feed(&tool_name, &tool_input, &tool_response)
            .expect("Grok Bash observation must yield a feed");
        let url = boss_engine_structured_output::pr_url::find_first_pr_url(&feed.output_text).expect("url");
        assert_eq!(url, "https://github.com/spinyfin/mono/pull/458");
    }

    #[test]
    fn pr_url_capture_feed_returns_none_for_non_bash_tool() {
        let tool_input = json!({"file_path": "/tmp/x.txt", "content": "hi"});
        let tool_response = json!({"type": "SearchReplace", "EditsApplied": {}});
        assert!(
            GrokDriver::default()
                .pr_url_capture_feed("Write", &tool_input, &tool_response)
                .is_none()
        );
    }

    #[test]
    fn pr_url_capture_feed_handles_missing_command_gracefully() {
        let feed = GrokDriver::default()
            .pr_url_capture_feed("Bash", &json!({}), &real_bash_tool_result())
            .expect("feed");
        assert_eq!(feed.command, "");
    }

    // ── structured_output_wiring (design T-18) ──────────────────────────

    #[test]
    fn structured_output_wiring_stays_on_the_env_file_contract() {
        let path = std::path::PathBuf::from("/tmp/boss-worker-output/exec_x.pr-url.json");
        let request = StructuredOutputRequest {
            kind: StructuredOutputKind::PrUrl,
            result_path: &path,
            schema: None,
        };
        let artifacts = GrokDriver::default().structured_output_wiring(&request).unwrap();
        assert_eq!(artifacts, default_structured_output_wiring(&request));
        // No native --json-schema flag adopted (T-18: evaluated, not wired).
        assert!(artifacts.extra_args.is_empty());
    }

    // ── TranscriptAccess ─────────────────────────────────────────────────────

    #[test]
    fn transcript_path_for_session_reads_transcript_path_field() {
        // A stamped path that is not under a provisioned `sessions` symlink
        // (and does not match the production `boss-grok-homes` layout) is
        // returned as-is. The durable rewrite is covered by
        // `transcript_store::tests::recorded_grok_transcript_path_survives_home_reclaim`.
        let raw = json!({
            "sessionId": "sess-1",
            "hookEventName": "stop",
            "transcriptPath": "/tmp/grok-home/sessions/%2Fprivate%2Ftmp/sess-1/updates.jsonl",
        });
        assert_eq!(
            GrokDriver::default().transcript_path_for_session(&raw).as_deref(),
            Some("/tmp/grok-home/sessions/%2Fprivate%2Ftmp/sess-1/updates.jsonl"),
        );
    }

    #[test]
    fn transcript_path_for_session_is_none_when_missing_or_empty() {
        let missing = json!({"sessionId": "sess-1"});
        assert_eq!(GrokDriver::default().transcript_path_for_session(&missing), None);

        let empty = json!({"transcriptPath": ""});
        assert_eq!(GrokDriver::default().transcript_path_for_session(&empty), None);
    }

    #[test]
    fn transcript_session_correlates_tool_call_and_tool_call_update() {
        let mut session = GrokDriver::default()
            .transcript_session()
            .expect("GrokDriver supplies a per-tail transcript session");

        let call = session.normalize_transcript_entry(json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "run_terminal_command",
                "rawInput": {"command": "echo hi"}
            }}
        }));
        assert_eq!(call["content"][0]["name"], "Bash");

        let result = session.normalize_transcript_entry(json!({
            "method": "session/update",
            "params": {"sessionId": "s1", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "rawOutput": {"exit_code": 0}
            }}
        }));
        assert_eq!(result["type"], "tool_result");
        assert_eq!(result["tool_name"], "Bash");
        assert_eq!(result["content"], r#"{"exit_code":0}"#);
        assert_eq!(result["is_error"], false);
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
        assert_eq!(d.model_menu.engine_default, "grok-4.6");
    }

    #[test]
    fn grok_model_menu_uses_current_default_with_three_rung_effort() {
        let driver = GrokDriver::default();
        let menu = &driver.descriptor().model_menu;
        // Live Grok / grok-4.6 accepts only low|medium|high. Large and
        // Max must land on `high` (Grok's ceiling), never on Claude/Codex's
        // `xhigh`/`max` — those fail the first turn in-pane after spawn.
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Trivial), Some("low"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Small), Some("medium"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Medium), Some("high"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Large), Some("high"));
        assert_eq!((menu.effort_value_for_level)(EffortLevel::Max), Some("high"));
        // Guard: do not re-introduce the Claude/Codex vocabulary on Grok.
        for level in [
            EffortLevel::Trivial,
            EffortLevel::Small,
            EffortLevel::Medium,
            EffortLevel::Large,
            EffortLevel::Max,
        ] {
            let v = (menu.effort_value_for_level)(level).expect("every level maps");
            assert!(
                matches!(v, "low" | "medium" | "high"),
                "Grok effort for {level:?} must be one of low|medium|high, got {v:?}",
            );
        }
        // Both reasoning modes use the provider's current default.
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Standard), "grok-4.6");
        assert_eq!((menu.model_for_reasoning)(ReasoningMode::Investigation), "grok-4.6");
        assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Fast), "grok-4.6");
        assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Balanced), "grok-4.6");
        assert_eq!((menu.review_model_for_tier)(ReviewModelTier::Strong), "grok-4.6");
        assert_eq!((menu.default_model_for_level)(EffortLevel::Trivial), "grok-4.6");
        assert_eq!((menu.default_model_for_level)(EffortLevel::Max), "grok-4.6");
        assert!(!(menu.model_requires_auto_permissions)("grok-4.6"));
        assert!((menu.model_belongs_to_driver)("grok-4.6"));
        assert!((menu.model_belongs_to_driver)("GROK-4.6"));
        // A Claude/Codex family alias must not be recognised as Grok's.
        assert!(!(menu.model_belongs_to_driver)("opus"));
        assert!(!(menu.model_belongs_to_driver)("gpt-5.6-sol"));
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
        assert!(!caps.provides(Capability::CommandOutcomeObservation));
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

        let plan = GrokDriver::default().spawn_invocation(spawn_request("grok-4.6", run_id));

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
            plan.env.iter().any(|d| matches!(
                d,
                EnvDirective::Set(k, v) if k == "GROK_AUTH_PATH" && v == &auth.display().to_string()
            )),
            "must delegate OAuth to the shared host auth path: {:?}",
            plan.env
        );
        assert!(
            plan.env
                .iter()
                .any(|d| matches!(d, EnvDirective::Unset(k) if k == "XAI_API_KEY")),
            "must disable inherited API-key fallback: {:?}",
            plan.env
        );
        for key in ["GH_CONFIG_DIR", "CUBE_DATA_DIR", "CUBE_CONFIG_DIR"] {
            assert!(
                plan.env
                    .iter()
                    .any(|d| matches!(d, EnvDirective::Set(actual, _) if actual == key)),
                "must delegate host {key}: {:?}",
                plan.env
            );
        }
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
        assert!(cmd.contains("grok-4.6"), "has model slug: {cmd}");
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
        // Not cosmetic posture: a finishing subagent emits a `session_end`
        // Boss cannot tell from the worker's own. See the rationale at the
        // `--no-subagents` call site and
        // `docs/investigations/grok-subagent-hook-attribution-2026-08-09.md`,
        // plus `grok::progress`'s
        // `a_subagents_session_end_is_indistinguishable_from_the_top_level_sessions`.
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
    async fn provision_workspace_creates_owned_home_with_shared_auth_and_trust() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("source-auth.json");
        write_fake_auth(&auth);

        // Skip live inspect: auth is fake and this host may not have network.
        // Layout + shared-auth semantics are the unit under test; live inspect is
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

        // Auth must not exist per run. GROK_AUTH_PATH delegates directly to
        // runtime.auth_source_path so refreshes share one credential + lock.
        let auth_dest = runtime.grok_home.join("auth.json");
        assert!(!auth_dest.exists(), "per-run auth.json must not be provisioned");
        assert_eq!(runtime.auth_source_path, auth);
        let plan = driver.spawn_invocation(spawn_request("grok-4.6", "run-prov-1"));
        assert!(plan.env.iter().any(|directive| matches!(
            directive,
            EnvDirective::Set(key, value)
                if key == "GROK_AUTH_PATH" && value == &runtime.auth_source_path.display().to_string()
        )));

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
        // T-12/T-13: vim mode must never be enabled — Esc does not cancel
        // in fullscreen vim-scrollback mode, which would silently break
        // the interrupt control verb.
        assert!(config.contains("[ui]"));
        assert!(config.contains("vim_mode = false"));

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
    async fn provision_workspace_bridges_login_keychain_when_present() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("source-auth.json");
        write_fake_auth(&auth);
        // Fake real HOME with a fake login keychain — the actual macOS
        // credential store `gh auth login` writes the OAuth token to.
        let real_home = tmp.path().join("real-home");
        let keychain_dir = real_home.join("Library").join("Keychains");
        fs::create_dir_all(&keychain_dir).unwrap();
        let keychain_file = keychain_dir.join("login.keychain-db");
        fs::write(&keychain_file, b"fake-keychain-bytes").unwrap();
        let _home = home_override(&real_home);
        let _guard = env_for_provision_with_home_override(&homes, &auth, true);

        let driver = GrokDriver::default();
        let state = driver
            .provision_workspace(&workspace, "hello prompt", "run-keychain-1")
            .await
            .expect("provision")
            .expect("Grok must return runtime state");
        let runtime = GrokRuntimeState::from_driver_runtime_state(&state).unwrap();

        let bridged = runtime
            .process_home
            .join("Library")
            .join("Keychains")
            .join("login.keychain-db");
        let meta = fs::symlink_metadata(&bridged).expect("bridged login keychain must exist");
        assert!(
            meta.file_type().is_symlink(),
            "login.keychain-db must be a symlink, never a copy"
        );
        assert_eq!(fs::read_link(&bridged).unwrap(), keychain_file);
    }

    #[tokio::test]
    async fn provision_workspace_skips_keychain_bridge_when_absent() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("source-auth.json");
        write_fake_auth(&auth);
        // Real HOME exists but has no login keychain (e.g. a non-macOS host).
        let real_home = tmp.path().join("real-home-no-keychain");
        fs::create_dir_all(&real_home).unwrap();
        let _home = home_override(&real_home);
        let _guard = env_for_provision_with_home_override(&homes, &auth, true);

        let driver = GrokDriver::default();
        let state = driver
            .provision_workspace(&workspace, "hello prompt", "run-keychain-2")
            .await
            .expect("provision")
            .expect("Grok must return runtime state");
        let runtime = GrokRuntimeState::from_driver_runtime_state(&state).unwrap();

        assert!(
            !runtime
                .process_home
                .join("Library/Keychains/login.keychain-db")
                .exists(),
            "must not create a login-keychain bridge when the host has no source keychain"
        );
    }

    #[test]
    fn spawn_invocation_exports_gh_config_dir_directive() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let run_id = "run-spawn-gh";
        let grok_home = grok_home_for_run(run_id).unwrap();
        fs::create_dir_all(&grok_home).unwrap();
        fs::write(
            grok_home.join("boss-session-id"),
            "11111111-2222-4333-8444-555555555555\n",
        )
        .unwrap();
        fs::write(grok_home.join("boss-workspace-path"), "/tmp/ws-spawn-gh\n").unwrap();

        let real_gh_config = tmp.path().join("real-gh-config");
        fs::create_dir_all(&real_gh_config).unwrap();
        let prior_gh = std::env::var_os("GH_CONFIG_DIR");
        // SAFETY: serialised by ENV_LOCK, held via _guard for this test's lifetime.
        unsafe { std::env::set_var("GH_CONFIG_DIR", &real_gh_config) };

        let plan = GrokDriver::default().spawn_invocation(spawn_request("grok-4.6", run_id));

        let expected = real_gh_config.display().to_string();
        assert!(
            plan.env
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, v) if k == "GH_CONFIG_DIR" && v == &expected)),
            "must export GH_CONFIG_DIR pointing at the real host gh config: {:?}",
            plan.env
        );

        match prior_gh {
            Some(v) => unsafe { std::env::set_var("GH_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("GH_CONFIG_DIR") },
        }
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
        // Need a real shared auth.json for the live OAuth preflight; it is
        // passed as GROK_AUTH_PATH and never copied into GROK_HOME.
        let host_auth = dirs_home_grok_auth();
        if !host_auth.exists() {
            eprintln!("no host ~/.grok/auth.json; skipping live inspect provision test");
            return;
        }

        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();

        let driver = GrokDriver::default();
        let layout_guard = env_for_provision(&homes, &host_auth, true);
        let state = driver
            .provision_workspace(&workspace, "live inspect prompt", "run-live-1")
            .await
            .expect("provisioned layout must succeed")
            .expect("Grok must return runtime state");
        let runtime = GrokRuntimeState::from_driver_runtime_state(&state).unwrap();
        drop(layout_guard);

        let _inspect_guard = env_for_provision(&homes, &host_auth, false);
        home::assert_grok_posture(&runtime.grok_home, &runtime.process_home, &workspace)
            .expect("live grok inspect posture must succeed");
    }

    fn dirs_home_grok_auth() -> PathBuf {
        if let Some(path) = std::env::var_os(GROK_AUTH_SOURCE_ENV).filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
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
            codex_sandbox_enforced: false,
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
            codex_sandbox_enforced: false,
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

    /// `write_permission_config` must render sandbox.toml off macOS, no OS
    /// profile on local macOS, and CLI structural deny rules on both.
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
            codex_sandbox_enforced: false,
        };

        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();

        let grok_home = grok_home_for_run(run_id).unwrap();
        if permissions::grok_sandbox_disabled(false) {
            assert!(
                !grok_home.join("boss-seatbelt.sb").exists(),
                "local Grok workers must not materialize a Boss Seatbelt profile"
            );
            let spawn = driver.spawn_invocation(spawn_request("grok-4.6", run_id));
            assert!(
                !spawn.command.contains("sandbox-exec"),
                "local macOS pane must launch without an outer Seatbelt: {}",
                spawn.command
            );
            assert!(spawn.command.starts_with("grok "), "{}", spawn.command);
        } else {
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
        }

        assert_eq!(artifacts.extra_args[0], "--sandbox");
        assert_eq!(
            artifacts.extra_args[1],
            if permissions::grok_sandbox_disabled(false) {
                "off"
            } else {
                "boss-workspace"
            }
        );
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

    /// Reviewer worker kind must select a read-only workspace posture in the
    /// platform sandbox artifact.
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
            codex_sandbox_enforced: false,
        };

        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();
        assert_eq!(artifacts.extra_args[0], "--sandbox");
        assert_eq!(
            artifacts.extra_args[1],
            if permissions::grok_sandbox_disabled(false) {
                "off"
            } else {
                "boss-read-only"
            }
        );
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == format!("Edit({})", workspace.display())),
            "reviewer must deny workspace Edit: {:?}",
            artifacts.extra_args
        );
        assert!(
            artifacts
                .extra_args
                .windows(2)
                .any(|w| w[0] == "--deny" && w[1] == format!("Edit({}/**)", workspace.display())),
            "reviewer must deny workspace Edit glob: {:?}",
            artifacts.extra_args
        );

        let grok_home = grok_home_for_run(run_id).unwrap();
        if permissions::grok_sandbox_disabled(false) {
            assert!(
                !grok_home.join("boss-seatbelt.sb").exists(),
                "local macOS reviewer workers must not materialize a Boss Seatbelt profile"
            );
        } else {
            let sandbox_toml = fs::read_to_string(grok_home.join("sandbox.toml")).unwrap();
            assert!(sandbox_toml.contains("[profiles.boss-read-only]"), "{sandbox_toml}");
            assert!(sandbox_toml.contains("extends = \"read-only\""), "{sandbox_toml}");
        }
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
            codex_sandbox_enforced: false,
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
                 (set it explicitly to run — this test makes real, billed grok-4.6 API calls)"
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
            eprintln!("no Grok auth source; skipping live permission-enforcement test");
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
            codex_sandbox_enforced: false,
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

        // No hooks/, no sandbox.toml: only base config + trust, so
        // the only lever in play is the `--deny` CLI flag under test.
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
        for rule in permissions::structural_deny_rules(None, crate::WorkerKind::Standard, None) {
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

    /// design T-23 acceptance (part 1/2, Phase 3 — review kind): the
    /// `boss-read-only` sandbox that denies workspace writes must still
    /// permit the write the `ReviewResult` file contract depends on. The
    /// engine resolves that artifact under the system temp dir
    /// ([`boss_engine_structured_output::default_dir`]), one of the paths
    /// the built-in `read-only` profile always keeps writable regardless of
    /// worker kind (`grok-permission-isolation-2026-07-27.md` §"`/tmp`
    /// always writable"), so the reviewer's structured-output write and its
    /// workspace-write denial are independent properties, not in tension.
    ///
    /// Runs the *actual* driver output (hooks + sandbox included, exactly as
    /// a real reviewer spawns) and checks both properties by real
    /// file-presence and a schema-validated parse via
    /// [`boss_pr_review::ReviewResult::from_json`] — not the model's
    /// self-report of either outcome.
    #[tokio::test]
    async fn write_permission_config_live_review_result_round_trips_under_reviewer_sandbox() {
        if !live_enforcement_test_enabled() {
            return;
        }

        let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for this test"));
        let scratch_root = real_home
            .join(".cache")
            .join("boss-grok-permcfg-live-review-result-test");
        let _cleanup = ScratchCleanup::new(&scratch_root);

        let homes = scratch_root.join("grok-homes");
        let workspace = scratch_root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes, &dirs_home_grok_auth(), true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-live-review-result-1";
        driver
            .provision_workspace(&workspace, "live review-result round-trip probe", run_id)
            .await
            .expect("provision must succeed");

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Reviewer,
            workspace_path: workspace.clone(),
            events_socket_path: scratch_root.join("boss-data").join("events.sock"),
            boss_event_path: scratch_root.join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-live-review".into(),
            execution_kind: "pr_review".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: None,
            checkleft_guard_script: None,
            codex_sandbox_enforced: false,
        };
        let artifacts = driver
            .write_permission_config(&input, scratch_root.as_path())
            .await
            .expect("write_permission_config must succeed");

        let grok_home = grok_home_for_run(run_id).unwrap();
        let process_home = process_home_for_run(run_id).unwrap();

        // The same real destination the engine resolves for every reviewer
        // run (not a stand-in scratch path), so the round trip is proven
        // against the actual production location.
        let output_dir = boss_engine_structured_output::default_dir();
        fs::create_dir_all(&output_dir).unwrap();
        let result_path =
            boss_engine_structured_output::path_for(&output_dir, run_id, StructuredOutputKind::ReviewResult);
        let _result_cleanup = FileCleanup(result_path.clone());
        let _ = fs::remove_file(&result_path);

        let probe_write = workspace.join("probe_write.txt");
        let _ = fs::remove_file(&probe_write);

        let review_json = r#"{"pr_url":"https://github.com/example/repo/pull/1","head_sha":"deadbeefcafe","summary":"Live round-trip probe: one regression finding.","revision_warranted":true,"findings":[{"severity":"high","category":"regression","file":"src/lib.rs","title":"Live round-trip probe finding","detail":"Probe-only finding used to exercise enum + regression_check derivation.","confidence":"high"}],"regression_check":{"performed":true,"suspected_deletions":[]}}"#;
        let prompt = format!(
            "Do exactly two things, in order, then stop.\n\
             1. Try to create a file named probe_write.txt in the current directory with content \
             SHOULD_NOT_EXIST, using a write tool or shell. Report the outcome honestly, including any \
             denial/permission error text.\n\
             2. Write exactly this JSON, verbatim and unmodified, to the absolute path {}:\n\n{}\n\n\
             Final line: LIVE_DONE",
            result_path.display(),
            review_json,
        );

        let output = run_grok_probe(&grok_home, &process_home, &workspace, &prompt, &artifacts.extra_args);
        eprintln!(
            "live review-result round-trip probe stdout: {}",
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
            "workspace write must still be denied by the boss-read-only sandbox profile: probe_write.txt was created"
        );

        let written = fs::read_to_string(&result_path).unwrap_or_else(|err| {
            panic!(
                "ReviewResult artifact must exist at the engine-resolved path {}: {err}",
                result_path.display()
            )
        });
        let parsed = boss_pr_review::ReviewResult::from_json(&written).unwrap_or_else(|err| {
            panic!("ReviewResult artifact must round-trip through the real parser: {err}\n{written}")
        });
        assert_eq!(parsed.pr_url, "https://github.com/example/repo/pull/1");
        assert!(parsed.revision_warranted);
        assert_eq!(parsed.findings.len(), 1);
        assert_eq!(parsed.findings[0].severity, boss_pr_review::ReviewFindingSeverity::High);
        assert_eq!(
            parsed.findings[0].category,
            boss_pr_review::ReviewFindingCategory::Regression
        );
        assert!(parsed.regression_check.performed);
        assert_eq!(parsed.regression_check.suspected_deletions.len(), 1);
    }

    /// design T-23 acceptance (part 2/2, Phase 3 — conflict-resolution
    /// kind): conflict resolution dispatches under `WorkerKind::Standard`
    /// (`worker_setup::worker_kind_for_execution` maps
    /// `ExecutionKind::ConflictResolution` to `Standard`, never `Reviewer`),
    /// which needs real write access to resolve the conflict. This is the
    /// positive-control complement to
    /// `write_permission_config_live_sandbox_denies_workspace_write` above:
    /// proves the permission profile `write_permission_config` selects for
    /// `Standard` genuinely *permits* a workspace write, so
    /// "conflict resolution has write access" is demonstrated rather than
    /// merely inferred from "it isn't Reviewer".
    #[tokio::test]
    async fn write_permission_config_live_standard_sandbox_allows_workspace_write() {
        if !live_enforcement_test_enabled() {
            return;
        }

        let real_home = PathBuf::from(std::env::var_os("HOME").expect("HOME must be set for this test"));
        let scratch_root = real_home
            .join(".cache")
            .join("boss-grok-permcfg-live-standard-write-test");
        let _cleanup = ScratchCleanup::new(&scratch_root);

        let homes = scratch_root.join("grok-homes");
        let workspace = scratch_root.join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes, &dirs_home_grok_auth(), true);

        let driver = GrokDriver::default();
        let run_id = "run-permcfg-live-standard-write-1";
        driver
            .provision_workspace(&workspace, "live standard write-access probe", run_id)
            .await
            .expect("provision must succeed");

        let input = PermissionInput {
            worker_kind: crate::WorkerKind::Standard,
            workspace_path: workspace.clone(),
            events_socket_path: scratch_root.join("boss-data").join("events.sock"),
            boss_event_path: scratch_root.join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-live-standard".into(),
            execution_kind: "conflict_resolution".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: None,
            checkleft_guard_script: None,
            codex_sandbox_enforced: false,
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
            "Create a file named probe_write.txt in the current directory with content SHOULD_EXIST, \
             using a write tool or shell. Report the outcome honestly. Final line: LIVE_DONE",
            &artifacts.extra_args,
        );
        eprintln!(
            "live standard-write probe stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.status.success(),
            "live grok invocation failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(
            probe_write.exists(),
            "conflict resolution's Standard worker kind must retain real workspace write access under its \
             selected permission profile: probe_write.txt was not created"
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

    /// Removes a single file on drop, on every exit path including a mid-test
    /// panic. Needed for artifacts written into the real, machine-wide
    /// `boss_engine_structured_output::default_dir()` — unlike a scratch root
    /// under our own `ScratchCleanup`, that directory is shared with live
    /// engine runs, so a leaked file there is not test-local cleanup debt.
    struct FileCleanup(PathBuf);
    impl Drop for FileCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
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
        let environment = GrokProcessEnvironment::resolve(grok_home, process_home, &home::resolve_grok_auth_source())
            .expect("live probe environment must resolve");
        environment.apply_to_command(&mut cmd);
        cmd.arg("-p")
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
            .arg("grok-4.6");
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.output().expect("grok -p invocation must spawn")
    }

    // ── ControlVerbs (design T-13) ──────────────────────────────────────

    #[test]
    fn control_verb_delivery_plans_match_the_design() {
        use crate::{InterruptDelivery, ProbeDelivery, ReapDelivery, StopDelivery};

        let driver = GrokDriver::default();
        assert_eq!(driver.probe(), ProbeDelivery::PaneText);
        assert_eq!(driver.interrupt(), InterruptDelivery::PaneEsc);
        assert_eq!(driver.stop(), StopDelivery::PaneCommand { command: "/quit" });
        assert_eq!(driver.reap(), ReapDelivery::ProcessGroup);
    }

    #[test]
    fn classify_error_delegates_to_grok_classifier_not_claude() {
        let driver = GrokDriver::default();
        // A Claude-vocabulary-only marker ("overloaded_error", no bare
        // "overloaded") must not accidentally classify via Claude's
        // classifier; Grok's own vocabulary is what must match here.
        assert_eq!(
            driver.classify_error("authentication_failed: invalid x-api-key"),
            WorkerErrorClass::Permanent
        );
        assert_eq!(
            driver.classify_error("rate_limit: too many requests"),
            WorkerErrorClass::Transient
        );
        assert_eq!(
            driver.classify_error("nothing recognisable"),
            WorkerErrorClass::Indeterminate
        );
    }

    // ── Turn-end recovery (design T-12) ─────────────────────────────────

    #[test]
    fn is_interrupt_recovery_turn_end_delegates_to_turn_end_recovery_module() {
        let driver = GrokDriver::default();
        assert!(driver.is_interrupt_recovery_turn_end(&json!({
            "type": "turn_ended",
            "outcome": "cancelled",
            "cancellation_category": "mid_turn_abort",
        })));
        assert!(!driver.is_interrupt_recovery_turn_end(&json!({"type": "turn_ended", "outcome": "completed"})));
    }

    #[tokio::test]
    async fn prepare_interrupt_recovery_snapshots_offset_zero_for_a_fresh_session() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let run_id = "run-interrupt-1";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();

        let snapshot = driver
            .prepare_interrupt_recovery(run_id)
            .expect("a provisioned run must yield a recovery snapshot");

        assert_eq!(snapshot.offset, 0, "events.jsonl does not exist yet; offset must be 0");
        assert!(!snapshot.session_id.is_empty());
        let grok_home = grok_home_for_run(run_id).unwrap();
        assert!(snapshot.events_path.starts_with(grok_home.join("sessions")));
        assert!(
            snapshot
                .events_path
                .ends_with(format!("{}/events.jsonl", snapshot.session_id))
        );
        assert!(snapshot.settle_window > std::time::Duration::ZERO);
    }

    #[tokio::test]
    async fn prepare_interrupt_recovery_snapshots_existing_file_length() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let auth = tmp.path().join("auth.json");
        write_fake_auth(&auth);
        let _guard = env_for_provision(&homes, &auth, true);

        let driver = GrokDriver::default();
        let run_id = "run-interrupt-2";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();

        // Simulate an in-flight session that has already written some
        // events.jsonl content before the interrupt is delivered. Build the
        // path from the *stamped* (canonicalised) workspace path, exactly as
        // `prepare_snapshot` does internally — the raw `workspace` var may
        // differ (e.g. a symlinked temp dir), so using it here would make
        // this test write to a path production code never reads.
        let grok_home = grok_home_for_run(run_id).unwrap();
        let session_id = read_session_id(&grok_home).unwrap();
        let stamped_workspace = read_workspace_path_stamp(&grok_home).unwrap();
        let events_path = turn_end_recovery::events_jsonl_path(&grok_home, &stamped_workspace, &session_id);
        fs::create_dir_all(events_path.parent().unwrap()).unwrap();
        let existing = b"{\"type\":\"turn_started\"}\n";
        fs::write(&events_path, existing).unwrap();

        let snapshot = driver.prepare_interrupt_recovery(run_id).unwrap();
        assert_eq!(snapshot.offset, existing.len() as u64);
        assert_eq!(snapshot.events_path, events_path);
    }

    #[test]
    fn prepare_interrupt_recovery_returns_none_for_an_unprovisioned_run() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_for_provision(tmp.path(), &tmp.path().join("auth.json"), true);
        let driver = GrokDriver::default();
        assert!(driver.prepare_interrupt_recovery("never-provisioned-run").is_none());
    }
}
