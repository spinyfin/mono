//! `AgyDriver` — Google Antigravity (`agy`) agent driver.
//!
//! Descriptor, capability set, Gemini-family model menu, per-run `HOME`
//! provisioning, per-kind permission rendering (`--settings`), PreToolUse
//! authorization, hook progress observation (conversation-id filtering for
//! undisableable subagents), transcript access: the per-run `brain/` tree
//! is linked into Boss's durable per-execution transcript store, the
//! `transcriptPath` the claimed parent stamps is resolved onto it, and the
//! native `transcript_full.jsonl` dialect is normalised, and the interactive
//! pane command (`pane_command`) with its `PaneMonitorSpec`. The pane runs
//! the shared initial-brief launcher, which withholds the brief until the
//! CLI's startup log shows the rendered posture and the guard hooks loaded.
//! Measured pane controls are live for explicit pins; `launch_ready` stays
//! false during the pinned-only rollout. Retention preserves transcript
//! evidence in the per-run HOME.
//!
//! Qualified against `agy` 1.5.7 or later. `auto_approve` hook verdicts
//! were not measured on 1.5.5 (permission / shared-HOME studies,
//! 2026-11-14).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use async_trait::async_trait;
use boss_engine_structured_output::StructuredOutputKind;
use boss_engine_structured_output::fallback::FallbackCandidate;
use boss_protocol::{NormalizeError, PaneMonitorSpec, WorkerEvent};
use boss_ssh_transport::shell_quote;

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, DriverRuntimeState, HookWiringDestination,
    InterruptDelivery, InterruptPlan, InterruptRecoverySnapshot, MidTurnPaneInput, ModelMenu, PermissionArtifacts,
    PermissionInput, ProgressFidelity, ProgressIngress, ProgressObservationConfig, ProgressObservationWiring,
    ProgressSessionConfig, ProgressSessionNormalizer, SettingsLogCheckSpec, SpawnPlan, SpawnRequest,
    ToolUseInterceptionConfig, ToolUseInterceptionWiring, TranscriptSessionNormalizer, TurnEnd, WorkerErrorClass,
};

mod classify_error;
mod environment;
mod home;
mod hooks;
mod model_menu;
mod pane_command;
mod pending_tools;
pub mod permissions;
mod progress;
pub use environment::{
    AGY_BIN_ENV, AGY_VERSION_TIMEOUT, DISABLE_AUTO_UPDATE_ENV, DISABLE_AUTO_UPDATE_VALUE, MIN_QUALIFIED_AGY_VERSION,
    parse_agy_version, run_agy_bounded, version_at_least,
};
pub use pending_tools::REFUSED_TOOL_PREFIX;
#[cfg(test)]
mod test_environment;
mod transcript;
mod turn_boundary;
mod turn_end_recovery;

use progress::AgyProgressSession;
use transcript::AgyTranscriptSession;

pub mod quota;
/// Manual authenticated rules-discovery conformance probe.
pub mod rules_probe;

use environment::{AgyProcessEnvironment, peek_agy_binary, skip_binary_assert, verify_cached_agy_binary};
pub(crate) use home::{AGY_HOMES_DIR_NAME, GEMINI_DIR, PROCESS_HOME_LEAF, STATE_DIR};
pub use home::{
    AGY_HOMES_ENV_TEST_LOCK, AGY_HOMES_ROOT_ENV, AGY_TRANSCRIPT_PATHS, AgyRuntimeState, assert_agy_home_safe_to_delete,
    contains_agy_transcripts,
};
use home::{
    agent_rules_path_for_run, agy_homes_root, control_dir_for_run, log_file_for_run, process_home_for_run,
    provision_agy_home, read_workspace_path_stamp, session_settings_path_for_run,
};

pub use hooks::{
    AuthorizationProbeReport, adapter_path, attestation_path, hooks_file_path, install_authorization, policy_path,
    probe_authorization, verify_attestation, verify_startup_authorization,
};

// ---------------------------------------------------------------------------
// Descriptor
// ---------------------------------------------------------------------------
//
// Model menu refresh path: `agy models` is the machine-readable source.
// Update catalogue entries and role choices in model_menu. Do not dispatch
// Claude, GPT-OSS, or orchestrator ids through this driver.

static AGY_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "agy",
    label: "Antigravity",
    binary: "agy",
    // Workspace-local leaf used only as the descriptor's config-dir name.
    // Per-run `agy` state lives under the Boss-owned process HOME; the
    // initial-prompt launcher lives in that run's `control/` directory.
    // Agent rules go to `<run container>/rules-root/AGENTS.md`, a rules-only
    // directory passed to agy with --add-dir.
    config_dir: ".agy",
    agent_rules_filename: "AGENTS.md",
    initial_prompt_filename: "initial-prompt.txt",
    model_menu: ModelMenu {
        resolve_model_effort: Some(model_menu::resolve_model_effort),
        engine_default: model_menu::STANDARD_FAMILY,
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

const AGY_AGENT_RULES_PREAMBLE: &str =
    agent_rules_preamble!("via Antigravity hooks under an engine-owned per-run HOME.", "");

/// Google Antigravity CLI driver.
///
/// Registered under the `"agy"` slug. Declares the v1 capability set from
/// the Antigravity driver design. Per-run `HOME` provisioning, permission
/// rendering, PreToolUse authorization, hook progress observation, transcript
/// access, pane controls, and the interactive pane command are live.
/// `launch_ready` stays false for the pinned-only rollout, so unpinned traffic
/// never reaches this driver. Retention preserves per-run transcript evidence.
#[derive(Default)]
pub struct AgyDriver {
    _private: (),
}

/// Refuse launch while guard hooks are unavailable.
pub fn refuse_agy_spawn(reason: &str) -> String {
    format!("printf '%s\\n' {} >&2; exit 1\n", shell_quote(reason))
}

fn refuse_agy_plan(env: Vec<crate::EnvDirective>, reason: &str) -> SpawnPlan {
    SpawnPlan {
        env,
        command: refuse_agy_spawn(reason),
    }
}

#[async_trait]
impl AgentDriver for AgyDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &AGY_DESCRIPTOR
    }

    fn launch_ready(&self) -> bool {
        false
    }

    fn capabilities(&self) -> CapabilitySet {
        // Capability declaration for AgyDriver (v1) — design doc
        // §Capability declaration. Every omission below is deliberate;
        // each notes its absence disposition and the supporting study evidence.
        //
        // Provided:
        //   Spawn, WorkspaceProvisioning, PermissionPolicy, ModelAndEffortMenu,
        //   ProgressObservation, ToolUseInterception (rewrite-or-deny),
        //   TurnBoundary, StructuredOutput, TranscriptAccess, ControlVerbs,
        //   PromptComposition.
        //
        // ToolUseInterception is rewrite-or-deny: PreToolUse rewrites
        // arguments through `overwrite` on both measured binaries (hook
        // surface study, "PreToolUse can rewrite arguments"). Authorization
        // is installed by `write_permission_config`, which proves the
        // installed chain with `verify_startup_authorization` before returning.
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

    fn pane_monitor_spec(&self) -> Option<PaneMonitorSpec> {
        // Measured on `agy` 1.5.5 in
        // `docs/investigations/ghostty-agy-pane-viability.md` ("Proposed
        // PaneMonitorSpec"); the status pill reads these before the first
        // hook event arrives, and the launcher reads them to decide when the
        // composer is ready for the brief. `Antigravity CLI` is deliberately
        // not an agent marker: it is on the sign-in, onboarding and trust
        // screens, and the banner scrolls away.
        Some(PaneMonitorSpec {
            agent_markers: vec![
                "? for shortcuts".into(),
                "esc to cancel".into(),
                "Press up to edit queued messages".into(),
            ],
            busy_markers: vec![
                "esc to cancel".into(),
                "Press up to edit queued messages".into(),
                "task(s) · /tasks".into(),
            ],
            starting_markers: vec!["Signing in...".into()],
            prompt_prefixes: vec![">".into()],
            idle_debounce_polls: 2,
            // Screens that answer to input. Text delivered while one is up
            // answers it: a pasted message plus Return approved a pending
            // command, and typed text starting with `3` saved a global
            // always-allow rule (permission study, section 8). The first four
            // are the approval dialogs; the rest are blocking startup screens,
            // including the sign-in menu whose default choice is Google OAuth.
            blocking_dialog_markers: vec![
                "Run this command?".into(),
                "Allow creation of this file?".into(),
                "Accept this file edit?".into(),
                "Allow access to this file?".into(),
                "Do you trust the contents of this project?".into(),
                "Settings Error".into(),
                "Select login method:".into(),
            ],
        })
    }

    fn launcher_starting_markers(&self) -> Vec<String> {
        vec![
            "Welcome to Antigravity CLI!".into(),
            "Choose your color scheme".into(),
            "Terms of Service & Data Use".into(),
            "Accessing workspace:".into(),
            "Do you trust the contents of this project?".into(),
            "You are currently not signed in".into(),
            "Select login method:".into(),
            // A settings file the CLI rejects stops here and waits. Listing it
            // keeps the brief held, so the launcher fails the run instead of
            // pasting into a screen that would read it as a menu answer.
            "Settings Error".into(),
        ]
    }

    fn post_submit_failure_markers(&self) -> Vec<String> {
        // Measured: the vendor eligibility service answering 500 discards the
        // submitted prompt on screen; the composer returns to idle and no
        // `SessionStart` or `Stop` hook fires.
        vec!["Eligibility check failed".into()]
    }

    fn settings_log_check(&self) -> Option<SettingsLogCheckSpec> {
        Some(SettingsLogCheckSpec {
            verify: permissions::verify_runtime_state,
        })
    }

    fn delivery_home(&self, workspace: &Path, run_id: &str) -> PathBuf {
        // The launcher writes markers next to the brief in the run's
        // control directory, not under the descriptor's `.agy` config-dir
        // name. Do not record that path as `initial_prompt_home` on
        // runtime state: owned-home reclamation would then treat the
        // control directory as a Codex/Claude home.
        control_dir_for_run(run_id)
            .unwrap_or_else(|_| crate::initial_prompt::delivery_home(&workspace.join(self.descriptor().config_dir)))
    }

    fn spawn_invocation(&self, request: SpawnRequest<'_>) -> SpawnPlan {
        let Some(run_id) = request.run_id.filter(|id| !id.is_empty()) else {
            return refuse_agy_plan(
                Vec::new(),
                "agy spawn refused: missing run_id; per-run HOME cannot be resolved",
            );
        };

        let process_home = match process_home_for_run(run_id) {
            Ok(path) => path,
            Err(error) => {
                return refuse_agy_plan(
                    Vec::new(),
                    &format!("agy spawn refused: cannot resolve per-run HOME: {error:#}"),
                );
            }
        };
        let log_file = log_file_for_run(run_id).unwrap_or_else(|_| process_home.join("../control/agy.log"));
        let session_settings = session_settings_path_for_run(run_id)
            .unwrap_or_else(|_| process_home.join("../control/session-settings.json"));
        let agy_binary = peek_agy_binary(&agy_homes_root()).unwrap_or_else(|_| PathBuf::from("/nonexistent/agy"));

        let environment = match AgyProcessEnvironment::resolve(&process_home, &log_file, &session_settings, agy_binary)
        {
            Ok(environment) => environment,
            Err(error) => {
                tracing::error!(run_id, error = %error, "refusing to spawn agy worker with unresolved host tool environment");
                return refuse_agy_plan(
                    Vec::new(),
                    &format!("agy worker environment resolution failed: {error:#}"),
                );
            }
        };
        let env = environment.directives();
        let workspace_stamp = control_dir_for_run(run_id)
            .ok()
            .and_then(|dir| read_workspace_path_stamp(&dir).ok());
        tracing::debug!(
            run_id,
            log_file = %environment.log_file().display(),
            session_settings = %environment.session_settings_path().display(),
            workspace = %workspace_stamp.as_deref().unwrap_or(Path::new(".")).display(),
            "agy spawn environment resolved"
        );
        let Some(workspace) = workspace_stamp else {
            return refuse_agy_plan(
                env,
                "agy spawn refused: missing workspace path stamp; cannot verify folder trust before launch",
            );
        };

        if !process_home.exists() {
            return refuse_agy_plan(env, "agy spawn refused: per-run HOME is not provisioned");
        }
        // Last check of the private settings, and it runs before the pane CLI
        // starts. A start drops default-valued keys; a retry re-provisions
        // first, so this is never a second plan against a home the CLI has
        // already started in. After launch the session mode comes from
        // `--settings`, which the CLI never writes back.
        if let Err(error) = home::assert_agy_posture(&process_home, &workspace) {
            return refuse_agy_plan(env, &format!("agy spawn refused: private HOME posture: {error:#}"));
        }
        if !skip_binary_assert()
            && let Err(error) = verify_cached_agy_binary(environment.agy_binary())
        {
            return refuse_agy_plan(
                env,
                &format!("agy spawn refused: Boss-owned binary identity check failed: {error:#}"),
            );
        }

        // The guard chain must be on disk with the bytes that were attested
        // when it was installed, immediately before the CLI starts.
        if let Err(error) = verify_attestation(&process_home) {
            return refuse_agy_plan(
                env,
                &format!("agy spawn refused: authorization chain is not installed or has changed: {error:#}"),
            );
        }
        let control_dir = control_dir_for_run(run_id).unwrap_or_else(|_| process_home.join("../control"));
        // The launcher, its configuration and the brief are written at
        // provisioning, and the settings-log check at permission rendering.
        // A missing one would otherwise surface as a launcher crash on a
        // pane nobody is watching.
        for required in [
            "initial-prompt.py",
            "initial-prompt.json",
            "initial-prompt.txt",
            "settings-log-check.json",
            "session-settings.json",
        ] {
            if !control_dir.join(required).is_file() {
                return refuse_agy_plan(
                    env,
                    &format!(
                        "agy spawn refused: {} is missing from the run control directory",
                        control_dir.join(required).display()
                    ),
                );
            }
        }

        // A home whose `brain/` is the durable-store link is freely
        // reclaimable; only a legacy home holding a real `brain/` or
        // `conversations/` is retained as the sole copy of its transcript.
        SpawnPlan {
            env,
            command: pane_command::build_agy_pane_command(&environment, &control_dir, request.model, request.effort),
        }
    }

    async fn provision_workspace(
        &self,
        workspace: &Path,
        prompt_text: &str,
        run_id: &str,
    ) -> anyhow::Result<Option<DriverRuntimeState>> {
        let (workspace_owned, prompt_owned, run_id_owned) =
            (workspace.to_path_buf(), prompt_text.to_owned(), run_id.to_owned());
        let runtime =
            tokio::task::spawn_blocking(move || provision_agy_home(&workspace_owned, &prompt_owned, &run_id_owned))
                .await
                .context("agy workspace provisioning task did not complete")?
                .with_context(|| format!("provisioning Boss-owned agy HOME for run_id {run_id:?}"))?;
        Ok(Some(runtime.to_driver_runtime_state()))
    }

    async fn teardown_workspace(
        &self,
        _workspace: Option<&Path>,
        _run_id: &str,
        runtime_state: Option<&DriverRuntimeState>,
    ) -> anyhow::Result<()> {
        let Some(state) = runtime_state else {
            return Ok(());
        };
        let runtime = AgyRuntimeState::from_driver_runtime_state(state)?;
        home::reclaim_agy_run(&runtime.process_home)?;
        Ok(())
    }

    /// Render the run's session settings (posture, trust, native-file rules)
    /// into its control directory (`--settings <file>`), then install the
    /// PreToolUse authorization and forwarder wiring into the per-run home's
    /// global-root `hooks.json`, overwriting the provisioned no-op canary.
    /// Settings are rendered first because the authorization attestation hashes
    /// them. The installed chain is then verified, and a chain that cannot be
    /// verified fails the call. `dest_dir` is unused: the artifacts live in the driver's own
    /// per-run container, outside the checkout and outside any engine settings
    /// directory, so the retention sweep reclaims them with the run. The
    /// control directory is deliberately not added to `trustedWorkspaces`.
    async fn write_permission_config(
        &self,
        input: &PermissionInput,
        _dest_dir: &Path,
    ) -> anyhow::Result<PermissionArtifacts> {
        let render_input = input.clone();
        let run_id = input.run_id.clone();
        let mut artifacts = tokio::task::spawn_blocking(move || permissions::write_session_settings(&render_input))
            .await
            .context("agy permission rendering task did not complete")?
            .with_context(|| {
                format!("AgyDriver::write_permission_config: rendering session settings for run_id {run_id:?}")
            })?;
        let process_home = process_home_for_run(&input.run_id).with_context(|| {
            format!(
                "AgyDriver::write_permission_config: resolving per-run HOME for run_id {:?}",
                input.run_id
            )
        })?;
        if !process_home.exists() {
            bail!(
                "AgyDriver::write_permission_config: process HOME {} does not exist; \
                 call provision_workspace first",
                process_home.display()
            );
        }
        // Session settings (`--settings`) were rendered above by the per-kind
        // permission renderer; the attestation hashes that file.
        home::validate_private_layout(&process_home)?;
        let obs = hooks::observation_from_permission(input);
        let interception = ToolUseInterceptionConfig::from_permission_input(input);
        let auth_files = hooks::install_authorization(&process_home, input.worker_kind, &obs, &interception)
            .with_context(|| {
                format!(
                    "installing Antigravity PreToolUse authorization under {}",
                    process_home.display()
                )
            })?;
        for file in auth_files {
            if !artifacts.config_files.contains(&file) {
                artifacts.config_files.push(file);
            }
        }
        // Prove the installed chain before any brief can be sent: the host
        // side of the guards, exercised end to end, with no reliance on a
        // native `SessionStart` (which only fires with the first prompt).
        // The CLI side (hooks discovered, settings initialized) is proven by
        // the launcher against the CLI's own startup log before it pastes.
        // A failure here fails the run loudly instead of launching unguarded.
        let verified_input = input.clone();
        tokio::task::spawn_blocking(move || verify_startup_authorization(&verified_input))
            .await
            .context("agy startup authorization verification task did not complete")?
            .with_context(|| {
                format!(
                    "AgyDriver::write_permission_config: startup authorization is not verifiable for run_id {:?}",
                    input.run_id
                )
            })?;
        Ok(artifacts)
    }

    fn progress_fidelity(&self) -> ProgressFidelity {
        // PreToolUse / PostToolUse fire in the interactive TUI (pane
        // viability study Q2). Declared Rich so the stale-worker sweep
        // threshold is correct once hook wiring lands.
        ProgressFidelity::Rich
    }

    fn progress_observation_wiring(&self, config: &ProgressObservationConfig) -> ProgressIngress {
        // Destination is DriverOwned: hooks are discovered from
        // `$HOME/.gemini/config/hooks.json`, never from the Claude worker
        // settings file. The map is the six-event forwarder wiring with
        // PreToolUse replaced by the authorization adapter. Must not return
        // StdoutJsonl — the registry refuses that topology for built-in
        // drivers.
        let hooks = hooks::authorization_hooks_map(config).unwrap_or_else(|error| {
            // The trait cannot return an error. Declare the forwarder events
            // only: `install_authorization` resolves the same paths and
            // refuses the spawn, so no worker runs without the guard.
            tracing::error!(run_id = %config.run_id, error = %error, "agy PreToolUse authorization wiring cannot be declared");
            hooks::forwarder_hooks_map(config)
        });
        ProgressIngress::HookCallback(ProgressObservationWiring {
            hooks,
            destination: HookWiringDestination::DriverOwned,
        })
    }

    fn normalize_progress_event(&self, raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        // Stateless decoding only supports self-describing non-boundary events.
        if matches!(
            raw.get("hookEventName")
                .or_else(|| raw.get("hook_event_name"))
                .and_then(serde_json::Value::as_str),
            Some("SessionStart" | "PreInvocation" | "Stop")
        ) {
            return Err(NormalizeError::Malformed(
                "agy boundary events require a per-run progress_session".into(),
            ));
        }
        AgyProgressSession::new(None, None).normalize_progress_event(raw)
    }

    fn progress_session(&self, config: &ProgressSessionConfig) -> Option<Box<dyn ProgressSessionNormalizer>> {
        Some(Box::new(AgyProgressSession::new(
            config.run_id.clone(),
            config.identity_store.clone(),
        )))
    }

    fn turn_boundary(&self, event: &WorkerEvent) -> Option<TurnEnd> {
        // The progress session emits Stop only for the claimed parent's idle boundary.
        turn_boundary::turn_boundary(event)
    }

    fn tool_use_interception_wiring(&self, _config: &ToolUseInterceptionConfig) -> ToolUseInterceptionWiring {
        // Dead in practice for agy: `progress_observation_wiring` declares
        // `HookWiringDestination::DriverOwned`, so the engine never merges
        // these entries into a Claude worker settings file. The real
        // PreToolUse adapter is installed by `write_permission_config`.
        ToolUseInterceptionWiring {
            pre_tool_use_hooks: Vec::new(),
        }
    }

    fn agent_rules_preamble(&self) -> &'static str {
        AGY_AGENT_RULES_PREAMBLE
    }

    /// Route Boss worker rules to `<rules-root>/AGENTS.md` under the per-run
    /// container, added to the CLI workspace with `--add-dir`.
    fn agent_rules_destination(&self, _workspace: &Path, run_id: &str) -> PathBuf {
        agent_rules_path_for_run(run_id)
            .unwrap_or_else(|_| agy_homes_root().join("unknown-run").join("rules-root/AGENTS.md"))
    }

    /// The transcript of the **claimed parent** conversation, from the payload
    /// that names it.
    ///
    /// Every `agy` hook payload carries `transcriptPath`, and every subagent
    /// has a transcript of its own (hook study, "Subagent identity"), so the
    /// field alone does not say whose transcript it is. The driver cannot
    /// consult the claimed identity here — this method is stateless — but the
    /// progress session already gates exactly three event kinds on it:
    /// `SessionStart`, `PreInvocation` and `Stop` are dropped before they reach
    /// the engine unless they carry the parent's `conversationId`. Tool hooks
    /// are not gated (a child's progress stays visible), so they never
    /// contribute a path: a child's `PostToolUse` must not become the run's
    /// transcript just because it arrived first. The engine keeps the first
    /// path it is given, and the parent's `SessionStart` always comes first.
    ///
    /// The path is the one the CLI stamped, never one constructed from a
    /// documentation example (the reference describes `transcript.jsonl`; the
    /// binary writes `transcript_full.jsonl`). Empty strings are treated as
    /// missing, matching Claude/Grok. [`persistable_transcript_path`]
    /// rewrites a path through the per-run `brain` link onto the durable store,
    /// so the recorded location survives the home's reclaim.
    ///
    /// [`persistable_transcript_path`]: crate::transcript_store::persistable_transcript_path
    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        let event = raw
            .get("hookEventName")
            .or_else(|| raw.get("hook_event_name"))
            .and_then(serde_json::Value::as_str)?;
        if !matches!(event, "SessionStart" | "PreInvocation" | "Stop") {
            return None;
        }
        let s = raw
            .get("transcriptPath")
            .or_else(|| raw.get("transcript_path"))
            .and_then(|v| v.as_str())?;
        if s.is_empty() {
            None
        } else {
            Some(
                crate::transcript_store::persistable_transcript_path(Path::new(s))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    fn transcript_session(&self) -> Option<Box<dyn TranscriptSessionNormalizer>> {
        // A tool call and its result are separate rows joined only by
        // position — `AgyTranscriptSession` owns that per-tail correlation.
        Some(Box::new(AgyTranscriptSession::default()))
    }

    fn transcript_containment_root(&self, run_id: &str) -> anyhow::Result<Option<PathBuf>> {
        home::transcript_containment_root(run_id)
    }

    fn normalize_transcript_entry(&self, raw: serde_json::Value) -> serde_json::Value {
        // Isolated single-row reshape for callers that hold no
        // `transcript_session()` tail; each call starts with no pending tool
        // call, so a result row is never paired here.
        transcript::normalize_transcript_entry(raw)
    }

    fn extract_error_from_transcript(&self, lines: &[serde_json::Value]) -> Option<String> {
        // The dialect work is in the normaliser, which flags a halting
        // `ERROR_MESSAGE` step with `isApiErrorMessage: true`. Honour only
        // that flag: the shared prose fallback treats any assistant text
        // beginning "API Error" as a halt, and PLANNER_RESPONSE prose is
        // reshaped into exactly that shape. A refused or failed *tool* step
        // is a `tool_result` there, so a worker that was denied a command
        // and then stopped is not mistaken for one wedged on an API error.
        boss_engine_transient_error::extract_flagged_worker_error(lines)
    }

    fn classify_error(&self, raw_output: &str) -> WorkerErrorClass {
        classify_error::classify_agy_error(raw_output)
    }

    // All typed delivery uses the engine's blocking-dialog guard and Enter
    // submit key; a probe must never answer an approval or trust dialog.
    fn probe(&self) -> super::ProbeDelivery {
        super::ProbeDelivery::PaneText
    }

    fn interrupt(&self) -> InterruptDelivery {
        InterruptDelivery::PaneEsc
    }

    // Pane study Q5: one Escape cancels in 0.3–0.9 s, without a Stop hook.
    // Wait for the private log observer (including its settle fallback),
    // never infer completion merely from having sent the key.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        Some(InterruptPlan {
            gesture: super::InterruptGesture {
                key: "Escape",
                presses: 1,
                press_interval: std::time::Duration::ZERO,
            },
            confirm_window: turn_end_recovery::SETTLE_WINDOW,
            max_attempts: 2,
            turn_end_evidence: super::TurnEndEvidence::RecoveryObserver,
        })
    }

    fn stop(&self) -> super::StopDelivery {
        // Pane study Q8: /quit exits with status 0.
        super::StopDelivery::PaneCommand { command: "/quit" }
    }

    fn reap(&self) -> super::ReapDelivery {
        super::ReapDelivery::ProcessGroup
    }

    fn prepare_interrupt_recovery(
        &self,
        run_id: &str,
        claimed_session_id: Option<&str>,
    ) -> Option<InterruptRecoverySnapshot> {
        turn_end_recovery::prepare_snapshot(run_id, claimed_session_id)
    }

    // This shape check is deliberately identity-free; is_interrupt_recovery_line
    // is the authoritative matcher against the persisted parent identity.
    fn is_interrupt_recovery_turn_end(&self, raw: &serde_json::Value) -> bool {
        raw.as_str()
            .and_then(turn_end_recovery::cancelled_conversation)
            .is_some()
    }

    fn is_interrupt_recovery_line(&self, line: &str, snapshot: &InterruptRecoverySnapshot) -> bool {
        turn_end_recovery::cancelled_conversation(line) == Some(snapshot.session_id.as_str())
    }

    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        // Pane viability study Q6: queued input runs at the next Stop, even
        // fullyIdle=false. The session's `--settings` pin queuedMessages to
        // queue; the private settings carry the same pin up to the launch.
        MidTurnPaneInput::Buffers
    }

    /// Prose fallback for the structured-output file contract.
    ///
    /// The conventions the engine teaches a worker for its final message —
    /// the PR URL on its own line, the `FOLLOWUPS:` block, the triage marker,
    /// the reviewer's fenced JSON — live in the driver-neutral worker brief,
    /// which an `agy` worker receives unchanged, and `agy`'s final message is
    /// the same plain prose (its last `PLANNER_RESPONSE`, normalised to an
    /// assistant text block). They are therefore declared for this driver
    /// explicitly rather than inherited by accident, and ride the one shared
    /// implementation. Every candidate is still validated against its kind's
    /// parser by the caller, so a worker that ignored the convention yields
    /// nothing, exactly as an empty list would. This is the fallback only: the
    /// env-file contract is primary and applies to every driver.
    fn structured_output_fallback(&self, kind: StructuredOutputKind, text: &str) -> Vec<FallbackCandidate> {
        crate::claude::structured_output::fallback_candidates(kind, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AbsenceDisposition;
    use crate::AgentDriver;
    use crate::EnvDirective;
    use crate::test_support::home_override;
    use boss_protocol::{EffortLevel, ReasoningMode, ReviewModelTier};
    use std::fs;
    use tempfile::TempDir;

    fn spawn_request<'a>(model: &'a str, effort: Option<&'a str>, run_id: Option<&'a str>) -> SpawnRequest<'a> {
        SpawnRequest {
            model,
            effort,
            settings_path: None,
            non_opus_auto_mode: false,
            permission_mode_override: None,
            run_id,
        }
    }

    use super::test_environment::{env_for_provision, env_for_provision_with_home_override};

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
        // Containment is the durable transcript store, which only exists once
        // a home has been provisioned; the provisioned case is covered below.
        assert_eq!(driver.transcript_containment_root("exec_contain").unwrap(), None);
        assert!(driver.settings_log_check().is_some());
        assert_eq!(d.model_menu.engine_default, model_menu::STANDARD_FAMILY);
        assert_eq!(
            (d.model_menu
                .design_investigation_model
                .expect("agy names a strong family"))(),
            model_menu::STRONG_FAMILY,
        );
    }

    #[test]
    fn delivery_home_is_the_run_control_dir_not_the_workspace_config_dir() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes);
        let run_id = "run-delivery-home";
        let driver = AgyDriver::default();
        let home = driver.delivery_home(&workspace, run_id);
        assert_eq!(home, control_dir_for_run(run_id).unwrap());
        assert_ne!(home, workspace.join(driver.descriptor().config_dir));

        let runtime = AgyRuntimeState {
            process_home: PathBuf::from("/tmp/process-home"),
            control_dir: PathBuf::from("/tmp/control"),
            session_settings_path: PathBuf::from("/tmp/control/session-settings.json"),
            log_file: PathBuf::from("/tmp/control/agy.log"),
            workspace_path: PathBuf::from("/tmp/ws"),
            agy_binary: PathBuf::from("/tmp/agy"),
        };
        assert!(
            crate::initial_prompt::owned_home_from_runtime_state(&runtime.to_driver_runtime_state()).is_none(),
            "the control dir must not be handed to owned-home reclamation"
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
                ReasoningMode::Standard => model_menu::STANDARD_FAMILY,
                ReasoningMode::Investigation => model_menu::STRONG_FAMILY,
            };
            assert_eq!(family, expected_family, "{reasoning:?}");
            for level in levels {
                let effort = (menu.effort_value_for_level)(level);
                let composed = model_menu::compose_model_id(family, effort);
                let rung = match level {
                    EffortLevel::Trivial => "low",
                    EffortLevel::Small => "medium",
                    EffortLevel::Medium => "high",
                    EffortLevel::Large | EffortLevel::Max if expected_family == model_menu::FAMILY_HYDROGEN => "xhigh",
                    EffortLevel::Large | EffortLevel::Max => "high",
                };
                let expected = format!("{expected_family}-{rung}");
                assert_eq!(composed, expected, "{reasoning:?} {level:?}");
            }
        }
    }

    #[test]
    fn review_and_legacy_tables_follow_the_family_choices() {
        let driver = AgyDriver::default();
        let menu = &driver.descriptor().model_menu;
        assert_eq!(
            (menu.review_model_for_tier)(ReviewModelTier::Fast),
            model_menu::STANDARD_FAMILY
        );
        assert_eq!(
            (menu.review_model_for_tier)(ReviewModelTier::Balanced),
            model_menu::STANDARD_FAMILY
        );
        assert_eq!(
            (menu.review_model_for_tier)(ReviewModelTier::Strong),
            model_menu::STRONG_FAMILY
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Trivial),
            model_menu::LIGHT_FAMILY
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Small),
            model_menu::STANDARD_FAMILY
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Medium),
            model_menu::STANDARD_FAMILY
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Large),
            model_menu::STRONG_FAMILY
        );
        assert_eq!(
            (menu.default_model_for_level)(EffortLevel::Max),
            model_menu::STRONG_FAMILY
        );
        assert!((menu.model_belongs_to_driver)("gemini-4-hydrogen-xhigh"));
        assert!((menu.model_belongs_to_driver)("GEMINI-4-CARBON-HIGH"));
        assert!((menu.model_belongs_to_driver)("gemini-3.8-flash-low"));
        assert!((menu.model_belongs_to_driver)("gemini-4-argon-medium"));
        assert!(!(menu.model_belongs_to_driver)("opus"));
        assert!(!(menu.model_belongs_to_driver)("claude-opus-5-5-high"));
        assert!(!(menu.model_belongs_to_driver)("gpt-oss-120b-medium"));
        assert!(!(menu.model_belongs_to_driver)("gemini-pro-orchestrator"));
        assert!(!(menu.model_requires_auto_permissions)(model_menu::STANDARD_FAMILY));
    }

    #[test]
    fn spawn_without_run_id_refuses_without_host_home() {
        let plan = AgyDriver::default().spawn_invocation(spawn_request(model_menu::STRONG_FAMILY, Some("xhigh"), None));
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&plan.command)
            .env("PATH", "/nonexistent")
            .output()
            .expect("run refusal command without any agy executable");
        assert_eq!(output.status.code(), Some(1));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("missing run_id"), "{stderr}");
        assert!(output.stdout.is_empty());
        assert!(plan.env.is_empty());
    }

    #[test]
    fn spawn_without_provision_exports_scoped_home_and_refuses() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let _guard = env_for_provision(&homes);
        let plan = AgyDriver::default().spawn_invocation(spawn_request(
            model_menu::FAMILY_CARBON,
            Some("xhigh"),
            Some("run-unprovisioned"),
        ));
        assert_eq!(
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&plan.command)
                .env("PATH", "/nonexistent")
                .output()
                .unwrap()
                .status
                .code(),
            Some(1)
        );
        let home = plan.env.iter().find_map(|d| match d {
            EnvDirective::Set(k, v) if k == "HOME" => Some(v.as_str()),
            _ => None,
        });
        let home = home.expect("must export scoped HOME");
        assert!(home.contains("run-unprovisioned"), "HOME={home}");
        assert!(home.contains("process-home"), "HOME={home}");
        assert!(
            plan.env
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, v) if k == "AGY_CLI_DISABLE_AUTO_UPDATE" && v == "true")),
            "must disable the updater: {:?}",
            plan.env
        );
    }

    #[tokio::test]
    async fn provision_workspace_creates_owned_home_and_exports_env() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let real_home = tmp.path().join("real-home");
        let keychain_dir = real_home.join("Library/Keychains");
        fs::create_dir_all(&keychain_dir).unwrap();
        fs::write(keychain_dir.join("login.keychain-db"), b"dummy-keychain").unwrap();
        let _home = home_override(&real_home);
        let _guard = env_for_provision_with_home_override(&homes);

        let driver = AgyDriver::default();
        let state = driver
            .provision_workspace(&workspace, "hello prompt", "run-prov-agy")
            .await
            .expect("provision")
            .expect("agy must return runtime state");
        let runtime = AgyRuntimeState::from_driver_runtime_state(&state).unwrap();
        assert!(runtime.process_home.starts_with(&homes));
        assert_ne!(runtime.process_home, real_home);
        assert!(
            runtime
                .process_home
                .join(".gemini/antigravity-cli/settings.json")
                .is_file()
        );
        assert!(!runtime.process_home.join(".gemini/settings.json").exists());
        let bridged = runtime.process_home.join("Library/Keychains/login.keychain-db");
        assert!(fs::symlink_metadata(&bridged).unwrap().file_type().is_symlink());

        let plan = driver.spawn_invocation(spawn_request(
            model_menu::FAMILY_CARBON,
            Some("xhigh"),
            Some("run-prov-agy"),
        ));
        assert!(plan.env.iter().any(
            |d| matches!(d, EnvDirective::Set(k, v) if k == "HOME" && v == &runtime.process_home.display().to_string())
        ));
        assert!(
            plan.env
                .iter()
                .any(|d| matches!(d, EnvDirective::Set(k, v) if k == "AGY_CLI_DISABLE_AUTO_UPDATE" && v == "true"))
        );
        let stderr = String::from_utf8(
            std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&plan.command)
                .output()
                .unwrap()
                .stderr,
        )
        .unwrap();
        assert!(stderr.contains("authorization chain is not installed"), "{stderr}");
        let fallback = runtime.process_home.join(".gemini/antigravity-cli/fallback-token.json");
        fs::write(&fallback, "private token").unwrap();
        let transcript = runtime
            .process_home
            .join(".gemini/antigravity-cli/brain/cid/.system_generated/logs/transcript_full.jsonl");
        fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        fs::write(&transcript, "conversation").unwrap();
        driver
            .teardown_workspace(Some(&workspace), "run-prov-agy", Some(&state))
            .await
            .unwrap();
        assert!(!fallback.exists());
        assert!(runtime.control_dir.is_dir(), "launch evidence survives teardown");
        assert!(
            !runtime.process_home.exists(),
            "a home with durable transcript storage keeps nothing of its own"
        );
        // The transcript was written through `brain/`; it now lives only in the
        // durable per-execution store, which outlives the reclaimed home.
        let durable = crate::transcript_store::durable_sessions_dir(
            &test_environment::transcript_store_for(&homes),
            "agy",
            "run-prov-agy",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(durable.join("cid/.system_generated/logs/transcript_full.jsonl")).unwrap(),
            "conversation"
        );
        assert_eq!(
            fs::read(keychain_dir.join("login.keychain-db")).unwrap(),
            b"dummy-keychain"
        );
    }

    #[test]
    fn spawn_refuses_unqualified_override_and_changed_binary() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let _guard = env_for_provision(&homes);
        provision_agy_home(tmp.path(), "prompt", "run-old").unwrap();
        let script = tmp.path().join("old-agy");
        fs::write(&script, "#!/bin/sh\necho 1.5.5\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let _binary_env = test_environment::EnvGuard::set_with_lock(
            &[
                (environment::AGY_SKIP_BINARY_ASSERT_ENV, "0"),
                (environment::AGY_BIN_ENV, script.to_str().unwrap()),
            ],
            None,
        );
        let plan = AgyDriver::default().spawn_invocation(spawn_request("model", None, Some("run-old")));
        assert!(
            plan.command.contains("binary identity check failed"),
            "{}",
            plan.command
        );
        let owned = homes.join("bin/agy");
        fs::create_dir_all(owned.parent().unwrap()).unwrap();
        fs::write(&owned, "#!/bin/sh\necho 1.5.7\n").unwrap();
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o555)).unwrap();
        environment::verify_agy_binary_with(&owned, |_| Ok(())).unwrap();
        fs::set_permissions(owned.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap();
        let qualified = AgyDriver::default().spawn_invocation(spawn_request("model", None, Some("run-old")));
        assert!(
            qualified.command.contains("authorization chain is not installed"),
            "{}",
            qualified.command
        );
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&owned, "#!/bin/sh\necho 1.5.5\n").unwrap();
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o555)).unwrap();
        let changed = AgyDriver::default().spawn_invocation(spawn_request("model", None, Some("run-old")));
        assert!(changed.command.contains("identity changed"), "{}", changed.command);
        fs::set_permissions(owned.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn agent_rules_destination_is_rules_only_directory() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_for_provision(tmp.path());
        let workspace = tmp.path().join("ws");
        let dest = AgyDriver::default().agent_rules_destination(&workspace, "run-agents-1");
        assert!(dest.ends_with("rules-root/AGENTS.md"), "dest={dest:?}");
        assert!(
            !dest.components().any(|c| c.as_os_str() == ".agy"),
            "must not write under workspace .agy: {dest:?}"
        );
        assert!(
            !dest.starts_with(&workspace),
            "must not write agent rules into the cube workspace: {dest:?}"
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
    fn measured_controls_are_available_to_pinned_workers() {
        let driver = AgyDriver::default();
        assert_eq!(driver.probe(), super::super::ProbeDelivery::PaneText);
        assert_eq!(driver.interrupt(), InterruptDelivery::PaneEsc);
        let plan = driver.interrupt_plan().unwrap();
        assert_eq!(plan.gesture.key, "Escape");
        assert_eq!(plan.gesture.presses, 1);
        assert_eq!(plan.max_attempts, 2);
        assert_eq!(plan.confirm_window, turn_end_recovery::SETTLE_WINDOW);
        assert_eq!(plan.turn_end_evidence, super::super::TurnEndEvidence::RecoveryObserver);
        assert_eq!(
            driver.stop(),
            super::super::StopDelivery::PaneCommand { command: "/quit" }
        );
        assert_eq!(driver.reap(), super::super::ReapDelivery::ProcessGroup);
        assert!(!driver.launch_ready());
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
                let pre = wiring
                    .hooks
                    .get("PreToolUse")
                    .and_then(|value| value.as_array())
                    .expect("PreToolUse authorization entry");
                assert_eq!(pre.len(), 1, "{pre:#?}");
                let command = pre[0]["hooks"][0]["command"].as_str().unwrap();
                assert!(command.contains("BOSS_AGY_POLICY="), "{command}");
                assert!(command.contains("boss-agy-hook-adapter.py"), "{command}");
                assert_eq!(wiring.hooks.len(), 6);
            }
            other => panic!("expected HookCallback(DriverOwned), got {other:?}"),
        }
        assert_eq!(AgyDriver::default().progress_fidelity(), ProgressFidelity::Rich);
        assert!(
            AgyDriver::default()
                .tool_use_interception_wiring(&ToolUseInterceptionConfig {
                    data_dir: None,
                    path_guard_script: None,
                    checkleft_guard_script: None,
                    git_store_guard_script: None,
                    is_revision: false,
                    is_standard_worker: true,
                    is_reviewer: false,
                    is_review_guide: false,
                    run_id: None,
                    workspace_path: None,
                })
                .pre_tool_use_hooks
                .is_empty()
        );
    }

    fn permission_input(tmp: &TempDir, run_id: &str, workspace: PathBuf) -> PermissionInput {
        crate::hook_guards::write_executable(&tmp.path().join("boss-event"), "#!/bin/sh\ncat >/dev/null\n").unwrap();
        let path_guard = tmp.path().join("boss-path-guard.py");
        fs::write(
            &path_guard,
            "#!/usr/bin/env python3\nimport json\nprint(json.dumps({'decision':'approve'}))\n",
        )
        .unwrap();
        PermissionInput {
            frontend_socket_path: None,
            worker_kind: crate::WorkerKind::Standard,
            workspace_path: workspace,
            events_socket_path: tmp.path().join("boss-data").join("events.sock"),
            boss_event_path: tmp.path().join("boss-event"),
            run_id: run_id.into(),
            lease_id: "lease-1".into(),
            execution_kind: "task_implementation".into(),
            task_kind: None,
            is_remote: false,
            path_guard_script: Some(path_guard),
            checkleft_guard_script: None,
            git_store_guard_script: None,
            codex_sandbox_enforced: false,
        }
    }

    #[tokio::test]
    async fn write_permission_config_requires_prior_provision() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let _guard = env_for_provision(&homes);
        let input = permission_input(&tmp, "run-no-provision", tmp.path().join("ws"));
        let err = AgyDriver::default()
            .write_permission_config(&input, tmp.path())
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("does not exist"),
            "expected missing-home error, got {err:#}"
        );
    }

    #[tokio::test]
    async fn write_permission_config_installs_pre_tool_use_authorization() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes);
        let driver = AgyDriver::default();
        let run_id = "run-agy-auth-1";
        driver.provision_workspace(&workspace, "hello", run_id).await.unwrap();
        let input = permission_input(&tmp, run_id, workspace);
        let artifacts = driver.write_permission_config(&input, tmp.path()).await.unwrap();
        let settings_path = session_settings_path_for_run(run_id).unwrap();
        assert_eq!(
            artifacts.extra_args,
            vec!["--settings".to_owned(), settings_path.display().to_string()],
            "session --settings is rendered by the permission renderer before guard installation"
        );
        assert!(artifacts.config_files.contains(&settings_path));
        let process_home = process_home_for_run(run_id).unwrap();
        let hooks_path = hooks_file_path(&process_home);
        let adapter = adapter_path(&process_home);
        assert!(
            artifacts.config_files.contains(&hooks_path),
            "config_files must include hooks.json: {:?}",
            artifacts.config_files
        );
        assert!(
            artifacts.config_files.contains(&adapter),
            "config_files must include the adapter: {:?}",
            artifacts.config_files
        );
        let hooks_doc: serde_json::Value = serde_json::from_str(&fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let pre = hooks_doc["boss"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "{pre:#?}");
        let command = pre[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains(&adapter.display().to_string()), "{command}");
        assert!(
            hooks_doc["boss"].get("SessionStart").is_some(),
            "SessionStart must forward progress after authorization install: {hooks_doc:#}"
        );
        assert!(!driver.launch_ready());
    }

    /// A host home for the live-probe tests. With `login_keychain`, provisioning
    /// has a credential to bridge into the run's home.
    fn host_home(tmp: &TempDir, login_keychain: bool) -> PathBuf {
        let home = tmp.path().join("host-home");
        fs::create_dir_all(home.join("Library/Keychains")).unwrap();
        if login_keychain {
            fs::write(home.join("Library/Keychains/login.keychain-db"), b"dummy-keychain").unwrap();
        }
        home
    }

    #[tokio::test]
    async fn provisioned_run_passes_startup_verification_after_the_cli_rewrites_its_settings() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _home = home_override(&host_home(&tmp, true));
        let _guard = test_environment::env_for_provision_with_live_probe(&homes);
        let driver = AgyDriver::default();
        let run_id = "run-agy-live-probe";

        let state = driver
            .provision_workspace(&workspace, "hello", run_id)
            .await
            .expect("provision with the live models probe")
            .expect("agy returns runtime state");
        let runtime = AgyRuntimeState::from_driver_runtime_state(&state).unwrap();

        // The CLI stand-in started once, in the home that is verified below,
        // and that start took the default-valued pin out of the settings.
        let starts = test_environment::fake_agy_starts(&homes);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(starts[0]["home"], runtime.process_home.display().to_string());
        assert_eq!(starts[0]["argv"].as_array().unwrap().last().unwrap(), "models");
        assert_eq!(starts[0]["dropped"], serde_json::json!(["queuedMessages"]));

        let input = permission_input(&tmp, run_id, workspace);
        driver
            .write_permission_config(&input, tmp.path())
            .await
            .expect("a provisioned run passes its own startup verification");
        assert!(verify_startup_authorization(&input).unwrap().ok);
        // The launch reads the settings Boss rendered, not the ones the CLI left.
        let settings = runtime.process_home.join(".gemini/antigravity-cli/settings.json");
        let private: serde_json::Value = serde_json::from_str(&fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(private["queuedMessages"], "queue");
        let plan = driver.spawn_invocation(spawn_request("gemini-4-carbon-high", None, Some(run_id)));
        assert!(plan.command.contains("initial-prompt.py"), "{}", plan.command);

        // The pin itself is not relaxed: a start after provisioning (the pane
        // launch is one) leaves a file that the same checks still refuse.
        // spawn_invocation is the last of those checks and runs before the
        // pane CLI starts; a second plan against that rewritten home, without
        // re-provisioning, is refused the same way.
        let mut started: serde_json::Value = private.clone();
        started.as_object_mut().unwrap().remove("queuedMessages");
        fs::write(&settings, serde_json::to_string_pretty(&started).unwrap()).unwrap();
        let error = verify_startup_authorization(&input).unwrap_err();
        assert!(
            format!("{error:#}").contains("must pin queuedMessages to queue (found no queuedMessages key"),
            "{error:#}"
        );
        let refused = driver.spawn_invocation(spawn_request("gemini-4-carbon-high", None, Some(run_id)));
        assert!(refused.command.starts_with("printf"), "{}", refused.command);
        assert!(refused.command.contains("private HOME posture"), "{}", refused.command);
        assert!(!refused.command.contains("initial-prompt.py"), "{}", refused.command);
    }

    #[tokio::test]
    async fn provisioning_fails_when_the_run_home_holds_no_credential() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        // No login keychain on the host, so none is bridged into the run's home.
        let _home = home_override(&host_home(&tmp, false));
        let _guard = test_environment::env_for_provision_with_live_probe(&homes);
        let driver = AgyDriver::default();
        let run_id = "run-agy-no-credential";

        let error = driver
            .provision_workspace(&workspace, "hello", run_id)
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("live agy models posture probe"), "{text}");
        assert!(text.contains("Please sign in to view available models"), "{text}");

        // The one CLI start was the non-interactive `models` probe in the
        // run's home. Nothing was launched to sign in, and no pane command
        // exists for a run that failed here.
        let starts = test_environment::fake_agy_starts(&homes);
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert_eq!(
            starts[0]["home"],
            process_home_for_run(run_id).unwrap().display().to_string()
        );
        assert_eq!(starts[0]["argv"].as_array().unwrap().last().unwrap(), "models");
        let plan = driver.spawn_invocation(spawn_request("gemini-4-carbon-high", None, Some(run_id)));
        assert!(plan.command.starts_with("printf"), "{}", plan.command);
        assert!(plan.command.contains("agy spawn refused"), "{}", plan.command);
    }
    // ── TranscriptAccess ────────────────────────────────────────────────────

    const TOOL_CALLS_CAPTURE: &str = include_str!("agy/testdata/tool_calls_1.5.9.jsonl");
    const DENIED_STEPS_CAPTURE: &str = include_str!("agy/testdata/denied_steps_1.5.9.jsonl");

    fn stamped_path(cid: &str) -> String {
        format!("/tmp/agy-test/brain/{cid}/.system_generated/logs/transcript_full.jsonl")
    }

    fn lifecycle_payload(event: &str, cid: &str) -> serde_json::Value {
        serde_json::json!({
            "hookEventName": event,
            "conversationId": cid,
            "modelName": "gemini-3.8-flash-low",
            "transcriptPath": stamped_path(cid),
            "executionNum": 0,
            "terminationReason": "NO_TOOL_CALL",
            "fullyIdle": true,
            "error": "",
        })
    }

    fn tool_payload(event: &str, cid: &str) -> serde_json::Value {
        serde_json::json!({
            "hookEventName": event,
            "conversationId": cid,
            "modelName": "gemini-3.8-flash-low",
            "transcriptPath": stamped_path(cid),
            "error": "",
            "toolCall": {"id": "call_1", "name": "run_command", "args": {"CommandLine": "true"}},
        })
    }

    #[test]
    fn transcript_path_comes_only_from_events_the_identity_filter_gates() {
        let driver = AgyDriver::default();
        for event in ["SessionStart", "PreInvocation", "Stop"] {
            assert_eq!(
                driver.transcript_path_for_session(&lifecycle_payload(event, "cid-1")),
                Some(stamped_path("cid-1")),
                "{event}"
            );
        }
        // Tool hooks are not identity-gated, so a child's would otherwise be
        // able to name its own transcript as the run's.
        for event in ["PreToolUse", "PostToolUse", "PostInvocation"] {
            assert_eq!(
                driver.transcript_path_for_session(&tool_payload(event, "cid-1")),
                None,
                "{event}"
            );
        }
        let mut no_event = lifecycle_payload("Stop", "cid-1");
        no_event.as_object_mut().unwrap().remove("hookEventName");
        assert_eq!(driver.transcript_path_for_session(&no_event), None);
        let mut empty = lifecycle_payload("Stop", "cid-1");
        empty["transcriptPath"] = serde_json::json!("");
        assert_eq!(driver.transcript_path_for_session(&empty), None);
        let mut snake = lifecycle_payload("Stop", "cid-1");
        let path = snake.as_object_mut().unwrap().remove("transcriptPath").unwrap();
        snake["transcript_path"] = path;
        assert_eq!(driver.transcript_path_for_session(&snake), Some(stamped_path("cid-1")));
    }

    #[test]
    fn a_subagent_never_supplies_the_runs_transcript() {
        // What the engine does with each payload, in order: ask the driver for a
        // path, run the payload through the per-connection progress session, and
        // use the path only if the session lets the event through.
        let driver = AgyDriver::default();
        let mut session = AgyProgressSession::new(None, None);
        let mut contributed = Vec::new();
        for payload in [
            lifecycle_payload("SessionStart", "parent"),
            tool_payload("PreToolUse", "parent"),
            // The child starts, works and stops. Its tool hooks pass the
            // session (so its progress stays visible); its boundaries do not.
            lifecycle_payload("SessionStart", "child"),
            lifecycle_payload("PreInvocation", "child"),
            tool_payload("PreToolUse", "child"),
            tool_payload("PostToolUse", "child"),
            lifecycle_payload("Stop", "child"),
            lifecycle_payload("Stop", "parent"),
        ] {
            let path = driver.transcript_path_for_session(&payload);
            let accepted = !session.normalize_progress_events(&payload).unwrap().is_empty();
            contributed.push(accepted.then_some(path).flatten());
        }
        assert_eq!(
            contributed.iter().flatten().collect::<Vec<_>>(),
            [&stamped_path("parent"), &stamped_path("parent")],
            "only the claimed parent's SessionStart and Stop name a transcript: {contributed:?}"
        );
    }

    #[test]
    fn transcript_session_pairs_a_call_with_its_result_across_rows() {
        let mut session = AgyDriver::default()
            .transcript_session()
            .expect("agy correlates tool calls across rows");
        let entries: Vec<serde_json::Value> = TOOL_CALLS_CAPTURE
            .lines()
            .map(|line| session.normalize_transcript_entry(serde_json::from_str(line).unwrap()))
            .collect();
        assert_eq!(entries[2]["type"], "tool_result");
        assert_eq!(entries[2]["tool_name"], "Bash");
        assert_eq!(entries[5]["content"][0]["text"], "CAPTURE_DONE");

        // The stateless entry point cannot pair rows, by design.
        let lone: serde_json::Value = serde_json::from_str(TOOL_CALLS_CAPTURE.lines().nth(2).unwrap()).unwrap();
        assert_eq!(
            AgyDriver::default().normalize_transcript_entry(lone)["subtype"],
            "generic_step"
        );
    }

    fn normalized_lines(driver: &AgyDriver, jsonl: &str) -> Vec<serde_json::Value> {
        let mut session = driver.transcript_session().unwrap();
        jsonl
            .lines()
            .map(|line| session.normalize_transcript_entry(serde_json::from_str(line).unwrap()))
            .collect()
    }

    #[test]
    fn a_trailing_halting_error_is_extracted_only_when_the_worker_did_not_recover() {
        let driver = AgyDriver::default();
        // Synthetic ERROR_MESSAGE row: the type is named by the CLI's step enum
        // but no halting error has been captured (see testdata/README.md).
        let error_row = r#"{"step_index":9,"source":"MODEL","type":"ERROR_MESSAGE","status":"ERROR","error":"model returned an error (code 429)"}"#;
        let recovered_row =
            r#"{"step_index":10,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"retrying"}"#;

        let halted = normalized_lines(&driver, &format!("{TOOL_CALLS_CAPTURE}{error_row}\n"));
        assert_eq!(
            driver.extract_error_from_transcript(&halted).as_deref(),
            Some("model returned an error (code 429)")
        );
        let recovered = normalized_lines(&driver, &format!("{TOOL_CALLS_CAPTURE}{error_row}\n{recovered_row}\n"));
        assert_eq!(driver.extract_error_from_transcript(&recovered), None);
    }

    #[test]
    fn a_refused_tool_step_is_not_a_halting_api_error() {
        // The real capture ends with two refused steps followed by the worker's
        // own reply, but even a transcript that ENDS on a refused step must not
        // read as a worker wedged on an API error: that would auto-resume a
        // worker a guard deliberately stopped.
        let driver = AgyDriver::default();
        let all = normalized_lines(&driver, DENIED_STEPS_CAPTURE);
        assert_eq!(driver.extract_error_from_transcript(&all), None);
        let ends_on_refusal = &all[..3];
        assert_eq!(ends_on_refusal.last().unwrap()["is_error"], true);
        assert_eq!(driver.extract_error_from_transcript(ends_on_refusal), None);
        assert_eq!(
            driver.extract_error_from_transcript(&normalized_lines(&driver, TOOL_CALLS_CAPTURE)),
            None
        );
    }

    #[test]
    fn planner_response_api_error_prose_is_not_a_halting_error() {
        let driver = AgyDriver::default();
        let row = r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","content":"API Error handling is now fixed"}"#;
        let lines = normalized_lines(&driver, &format!("{row}\n"));
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(
            boss_engine_transient_error::extract_worker_error(&lines).as_deref(),
            Some("API Error handling is now fixed"),
            "shared Claude-shaped fallback still matches the normalised prose"
        );
        assert_eq!(
            driver.extract_error_from_transcript(&lines),
            None,
            "agy must honour only flagged ERROR_MESSAGE rows"
        );
    }

    #[test]
    fn structured_output_fallback_declares_the_shared_final_message_conventions() {
        use boss_engine_structured_output::StructuredOutputKind as Kind;
        let driver = AgyDriver::default();
        let pr = "Opened it.\nhttps://github.com/spinyfin/mono/pull/123\n";
        let followups =
            "Summary.\n\nFOLLOWUPS:\n```json\n[{\"proposed_name\":\"n\",\"proposed_description\":\"d\"}]\n```\n";
        let review = "Review.\n```json\n{\"verdict\":\"approve\"}\n```\n";
        for (kind, text) in [
            (Kind::PrUrl, pr),
            (Kind::Followups, followups),
            (Kind::ReviewResult, review),
        ] {
            let candidates = driver.structured_output_fallback(kind, text);
            assert!(!candidates.is_empty(), "{kind:?} has a declared convention");
            assert_eq!(
                candidates,
                crate::claude::structured_output::fallback_candidates(kind, text),
                "{kind:?}: one shared implementation, not a divergent copy"
            );
        }
        assert!(
            driver
                .structured_output_fallback(Kind::PrUrl, "no url in this message")
                .is_empty()
        );
        // The postmortem manifest has never had a prose sentinel.
        assert!(
            driver
                .structured_output_fallback(Kind::PostmortemFollowups, followups)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn recorded_transcript_path_survives_home_reclaim() {
        // The lifecycle event that matters: the per-run home is reclaimed at
        // teardown, and the path recorded from the Stop payload must still
        // resolve — and still be inside the containment root — afterwards.
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let _guard = env_for_provision(&homes);
        let driver = AgyDriver::default();
        let run_id = "run-transcript-lifecycle";
        let state = driver
            .provision_workspace(&workspace, "prompt", run_id)
            .await
            .unwrap()
            .unwrap();
        let runtime = AgyRuntimeState::from_driver_runtime_state(&state).unwrap();

        let stamped = runtime
            .process_home
            .join(".gemini/antigravity-cli/brain/cid-1/.system_generated/logs/transcript_full.jsonl");
        fs::create_dir_all(stamped.parent().unwrap()).unwrap();
        fs::write(&stamped, "{\"step_index\":0}\n").unwrap();

        let recorded = driver
            .transcript_path_for_session(&serde_json::json!({
                "hookEventName": "Stop",
                "conversationId": "cid-1",
                "transcriptPath": stamped.to_string_lossy(),
            }))
            .expect("Stop stamps transcriptPath");
        assert_ne!(
            Path::new(&recorded),
            stamped.as_path(),
            "the recorded path must not run through the per-run home"
        );
        let root = driver
            .transcript_containment_root(run_id)
            .unwrap()
            .expect("a provisioned home is contained to its durable store");
        assert!(Path::new(&recorded).starts_with(&root), "{recorded} outside {root:?}");

        driver
            .teardown_workspace(Some(&workspace), run_id, Some(&state))
            .await
            .unwrap();
        assert!(!stamped.exists(), "precondition: reclaim deleted the per-run home");
        assert_eq!(fs::read_to_string(&recorded).unwrap(), "{\"step_index\":0}\n");
        assert_eq!(
            driver.transcript_containment_root(run_id).unwrap(),
            Some(root),
            "containment still resolves once the home is gone"
        );
    }

    #[test]
    fn a_run_that_never_provisioned_a_home_has_no_containment_root() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_for_provision(&tmp.path().join("homes"));
        assert_eq!(
            AgyDriver::default()
                .transcript_containment_root("run-never-provisioned")
                .unwrap(),
            None
        );
    }

    /// Provision, arm and return the artifacts the spawn flow merges into the command.
    async fn armed_run(tmp: &TempDir, run_id: &str) -> PermissionArtifacts {
        let workspace = tmp.path().join("ws");
        fs::create_dir_all(&workspace).unwrap();
        let driver = AgyDriver::default();
        driver
            .provision_workspace(&workspace, "the brief", run_id)
            .await
            .unwrap();
        let input = permission_input(tmp, run_id, workspace);
        driver.write_permission_config(&input, tmp.path()).await.unwrap()
    }

    #[tokio::test]
    async fn launch_argv_conforms_and_failed_cli_evidence_survives_teardown() {
        use crate::initial_prompt;
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        // Exercise shell quoting, including the permission-argument merge.
        let homes = tmp.path().join("homes with ' spaces");
        let _guard = env_for_provision(&homes);
        let binary = homes.join("bin/agy");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, include_str!("agy/testdata/launch.py")).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        for reject in [false, true] {
            let run_id = if reject { "run-rejected" } else { "run-composer" };
            let artifacts = armed_run(&tmp, run_id).await;
            fs::write(agent_rules_path_for_run(run_id).unwrap(), "Fixture worker rules").unwrap();
            let plan = AgyDriver::default().spawn_invocation(spawn_request(
                "gemini-4-hydrogen-medium",
                Some("medium"),
                Some(run_id),
            ));
            let command = crate::apply_permission_extra_args(&plan.command, &artifacts.extra_args);
            let mut shell = std::process::Command::new("/bin/sh");
            shell.arg("-c").arg(&command).stdin(std::process::Stdio::null());
            for directive in plan.env {
                match directive {
                    EnvDirective::Set(key, value) => {
                        shell.env(key, value);
                    }
                    EnvDirective::Unset(key) => {
                        shell.env_remove(key);
                    }
                }
            }
            shell.env_remove("AGY_FAKE_REJECT");
            if reject {
                shell.env("AGY_FAKE_REJECT", "1");
            }
            let output = crate::bounded_command::run_bounded_as(
                &mut shell,
                std::time::Duration::from_secs(15),
                "fake agy launch",
            )
            .unwrap();
            let control = control_dir_for_run(run_id).unwrap();
            if reject {
                assert_eq!(output.status.code(), Some(initial_prompt::BRIEF_NOT_DELIVERED_EXIT));
                let diagnostics = initial_prompt::startup_failure_diagnostics(&control).unwrap();
                assert!(diagnostics.contains("CLI exit status: 2"), "{diagnostics}");
                assert!(diagnostics.contains("rejected invocation sentinel"), "{diagnostics}");
                assert!(!initial_prompt::delivered_marker_present(&control));
                home::reclaim_agy_run(&process_home_for_run(run_id).unwrap()).unwrap();
                assert!(control.join("agy.log").is_file());
                assert!(control.join("session-settings.json").is_file());
                assert_eq!(
                    initial_prompt::startup_failure_diagnostics(&control).unwrap(),
                    diagnostics
                );
            } else {
                assert!(output.status.success(), "{output:?}\n{command}");
                assert!(initial_prompt::delivered_marker_present(&control));
                assert!(initial_prompt::startup_failure_diagnostics(&control).is_none());
            }
        }
    }

    #[tokio::test]
    async fn armed_run_spawns_the_launcher_with_settings_scoped_home_and_no_prompt_in_argv() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let _guard = env_for_provision(&homes);
        let run_id = "run-agy-spawn-1";
        let artifacts = armed_run(&tmp, run_id).await;

        let plan = AgyDriver::default().spawn_invocation(spawn_request(
            "gemini-4-hydrogen-xhigh",
            Some("xhigh"),
            Some(run_id),
        ));
        let command = crate::apply_permission_extra_args(&plan.command, &artifacts.extra_args);

        let control = control_dir_for_run(run_id).unwrap();
        assert!(
            command.starts_with(&format!("python3 -I '{}'/initial-prompt.py ", control.display())),
            "{command}"
        );
        assert!(
            command.contains("--model 'gemini-4-hydrogen-xhigh' --effort 'xhigh'"),
            "{command}"
        );
        let settings = session_settings_path_for_run(run_id).unwrap();
        assert!(
            command.contains(&format!("'--settings' '{}'", settings.display())),
            "--settings must come from the rendered permission artifacts: {command}"
        );
        assert!(
            command.contains(&format!(
                "'--add-dir' '{}'",
                control.parent().unwrap().join("rules-root").display()
            )),
            "{command}"
        );
        assert!(command.contains("'--log-file'"), "{command}");
        assert!(!command.contains("the brief"), "brief must never reach argv: {command}");
        assert!(!command.contains(" -i "), "{command}");
        assert_eq!(command.matches('\n').count(), 1, "one command line: {command:?}");

        let process_home = process_home_for_run(run_id).unwrap();
        for (key, value) in [
            ("HOME", process_home.display().to_string()),
            ("AGY_CLI_DISABLE_AUTO_UPDATE", "true".to_owned()),
            ("AGY_CLI_HIDE_ACCOUNT_INFO", "true".to_owned()),
        ] {
            assert!(
                plan.env
                    .iter()
                    .any(|d| matches!(d, EnvDirective::Set(k, v) if k == key && v == &value)),
                "{key}: {:?}",
                plan.env
            );
        }
        // The brief the launcher will paste lives in the control directory, outside every checkout.
        assert_eq!(
            fs::read_to_string(control.join("initial-prompt.txt")).unwrap(),
            "the brief"
        );
        // The launcher is told the status markers and the dialogs to refuse.
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(control.join("initial-prompt.json")).unwrap()).unwrap();
        assert!(
            config["blocking_dialog_markers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "Run this command?")
        );
        assert!(
            config["starting_markers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m == "Settings Error")
        );
        assert_eq!(config["paste_landed_markers"], serde_json::json!([]));
        assert_eq!(
            config["post_submit_failure_markers"],
            serde_json::json!(["Eligibility check failed"])
        );
        assert_eq!(config["check_settings_log"], true);
    }

    #[tokio::test]
    async fn spawn_refuses_when_the_authorization_chain_changed_after_installation() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let _guard = env_for_provision(&homes);
        let run_id = "run-agy-spawn-tamper";
        armed_run(&tmp, run_id).await;
        let hooks = hooks_file_path(&process_home_for_run(run_id).unwrap());
        fs::write(&hooks, "{}").unwrap();

        let plan = AgyDriver::default().spawn_invocation(spawn_request("gemini-4-carbon-high", None, Some(run_id)));
        assert!(plan.command.starts_with("printf"), "{}", plan.command);
        assert!(
            plan.command
                .contains("authorization chain is not installed or has changed"),
            "{}",
            plan.command
        );
        assert!(!plan.command.contains("initial-prompt.py"), "{}", plan.command);
    }

    #[tokio::test]
    async fn spawn_refuses_when_the_settings_log_check_was_never_written() {
        let tmp = TempDir::new().unwrap();
        let homes = tmp.path().join("homes");
        let _guard = env_for_provision(&homes);
        let run_id = "run-agy-spawn-no-check";
        armed_run(&tmp, run_id).await;
        fs::remove_file(control_dir_for_run(run_id).unwrap().join("settings-log-check.json")).unwrap();

        let plan = AgyDriver::default().spawn_invocation(spawn_request("gemini-4-carbon-high", None, Some(run_id)));
        assert!(
            plan.command.contains("settings-log-check.json is missing"),
            "{}",
            plan.command
        );
        assert!(!plan.command.contains("initial-prompt.py"), "{}", plan.command);
    }

    #[test]
    fn pane_monitor_spec_carries_the_measured_markers() {
        let spec = AgyDriver::default().pane_monitor_spec().unwrap();
        assert_eq!(
            spec.agent_markers,
            ["? for shortcuts", "esc to cancel", "Press up to edit queued messages"]
        );
        assert_eq!(
            spec.busy_markers,
            ["esc to cancel", "Press up to edit queued messages", "task(s) · /tasks"]
        );
        assert_eq!(spec.starting_markers, ["Signing in..."]);
        assert_eq!(spec.prompt_prefixes, [">"]);
        assert_eq!(spec.idle_debounce_polls, 2);
        assert!(
            !spec.agent_markers.iter().any(|m| m == "Antigravity CLI"),
            "the banner appears on onboarding screens and scrolls away"
        );
        for dialog in [
            "Run this command?",
            "Allow creation of this file?",
            "Accept this file edit?",
            "Allow access to this file?",
            "Do you trust the contents of this project?",
            "Settings Error",
            "Select login method:",
        ] {
            assert!(spec.blocking_dialog_markers.iter().any(|m| m == dialog), "{dialog}");
        }
        let driver = AgyDriver::default();
        assert_eq!(driver.blocking_dialog_markers(), spec.blocking_dialog_markers);
        let starting = driver.launcher_starting_markers();
        for screen in [
            "Do you trust the contents of this project?",
            "Settings Error",
            "Select login method:",
        ] {
            assert!(starting.iter().any(|m| m == screen), "{screen}");
        }
        assert!(driver.paste_landed_markers().is_empty());
    }
}
