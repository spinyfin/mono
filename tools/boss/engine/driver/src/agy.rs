//! `AgyDriver` — Google Antigravity (`agy`) agent driver.
//!
//! Descriptor, capability set, Gemini-family model menu, per-run `HOME`
//! provisioning, per-kind permission rendering (`--settings`), and
//! PreToolUse authorization. Interactive TUI spawn still refuses until the
//! startup contract lands. Retention preserves transcript evidence in the
//! per-run HOME.
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
    InterruptDelivery, InterruptPlan, MidTurnPaneInput, ModelMenu, PermissionArtifacts, PermissionInput,
    ProgressFidelity, ProgressIngress, ProgressObservationConfig, ProgressObservationWiring, SettingsLogCheckSpec,
    SpawnPlan, SpawnRequest, ToolUseInterceptionConfig, ToolUseInterceptionWiring, TurnEnd, WorkerErrorClass,
};

mod environment;
mod home;
mod hooks;
mod model_menu;
pub mod permissions;
#[cfg(test)]
mod test_environment;
mod transcript;

/// Manual authenticated rules-discovery conformance probe.
pub mod rules_probe;

use environment::{AgyProcessEnvironment, peek_agy_binary, skip_binary_assert, verify_cached_agy_binary};
pub(crate) use home::AGY_HOMES_DIR_NAME;
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
    probe_authorization, verify_attestation,
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
    // Agent rules go to `<run container>/rules-root/.agents/rules/boss.md`, a rules-only
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
/// rendering and PreToolUse authorization are live; interactive TUI spawn
/// still refuses until the startup contract lands. Retention preserves per-run
/// transcript evidence.
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
        // is installed by `write_permission_config`; TUI spawn still refuses
        // until the startup contract lands.
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
        // Measured in `docs/investigations/ghostty-agy-pane-viability.md`
        // ("Proposed PaneMonitorSpec"). Needed here so provision can write
        // the initial-prompt launcher config; TUI spawn still refuses.
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
            blocking_dialog_markers: vec![],
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
        ]
    }

    fn settings_log_check(&self) -> Option<SettingsLogCheckSpec> {
        Some(SettingsLogCheckSpec {
            verify: permissions::verify_runtime_state,
        })
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

        if !process_home.exists() {
            return refuse_agy_plan(env, "agy spawn refused: per-run HOME is not provisioned");
        }
        if let Err(error) =
            home::assert_agy_posture(&process_home, workspace_stamp.as_deref().unwrap_or(Path::new(".")))
        {
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

        // HOME is isolated; spawn stays refused until guard hooks land.
        // Retention preserves containers holding brain/ or conversations/
        // evidence while agy lacks durable transcript storage.
        refuse_agy_plan(env, "agy driver is not spawnable yet (guard hooks are not wired)")
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
    /// them. `dest_dir` is unused: the artifacts live in the driver's own
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
        // settings file. This map declares the PreToolUse authorization
        // entry; other lifecycle events are installed by the global-root
        // progress forwarder. Must not return StdoutJsonl — the registry
        // refuses that topology for built-in drivers.
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

    fn normalize_progress_event(&self, _raw: &serde_json::Value) -> Result<WorkerEvent, NormalizeError> {
        Err(NormalizeError::Malformed(
            "agy progress observation is not wired in this skeleton".into(),
        ))
    }

    fn turn_boundary(&self, _event: &WorkerEvent) -> Option<TurnEnd> {
        None
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

    /// Route Boss worker rules to `<rules-root>/.agents/rules/boss.md` under the
    /// per-run container, added to the CLI workspace with `--add-dir`.
    fn agent_rules_destination(&self, _workspace: &Path, run_id: &str) -> PathBuf {
        agent_rules_path_for_run(run_id).unwrap_or_else(|_| {
            agy_homes_root()
                .join("unknown-run")
                .join("rules-root/.agents/rules/boss.md")
        })
    }

    fn transcript_path_for_session(&self, raw: &serde_json::Value) -> Option<String> {
        // `agy` stamps `transcriptPath` on the Stop payload (pane study).
        // Empty strings are treated as missing, matching Claude/Grok.
        // `persistable_transcript_path` rewrites onto the durable store
        // when one exists, and warns when the stamp is still under a
        // per-run home that reclaim will delete.
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
        InterruptDelivery::Unsupported
    }

    // Pane viability study Q6: one Esc cancels and skips Stop. The plain-text
    // --log-file line is "Cancelling in-progress response for conversation <cid>".
    // No JSONL recovery source exists, so interrupts remain unsupported.
    fn interrupt_plan(&self) -> Option<InterruptPlan> {
        None
    }

    fn mid_turn_pane_input(&self) -> MidTurnPaneInput {
        // Pane viability study Q5: mid-turn stdin is buffered as the next
        // prompt. Override the safe Rejects default with that measurement.
        MidTurnPaneInput::Buffers
    }

    fn structured_output_fallback(&self, _kind: StructuredOutputKind, _text: &str) -> Vec<FallbackCandidate> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AbsenceDisposition;
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
        assert!(stderr.contains("guard hooks are not wired"), "{stderr}");
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
        assert!(!runtime.control_dir.exists());
        assert_eq!(fs::read_to_string(&transcript).unwrap(), "conversation");
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
            qualified.command.contains("guard hooks are not wired"),
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
        assert!(dest.ends_with("rules-root/.agents/rules/boss.md"), "dest={dest:?}");
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
    fn interrupts_are_unsupported_without_recovery() {
        let driver = AgyDriver::default();
        assert_eq!(driver.interrupt(), InterruptDelivery::Unsupported);
        assert!(driver.interrupt_plan().is_none());
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
}
