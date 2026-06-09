//! `ClaudeDriver` — the reference implementation of [`AgentDriver`] for
//! Claude Code. The `Spawn` capability is live; remaining behavioural methods
//! are `unimplemented!()` pending their per-capability extraction tasks
//! (Depth 1–2 in the design).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{AgentDriver, Capability, CapabilitySet, DriverDescriptor, PermissionInput, WorkerErrorClass};

static CLAUDE_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "claude",
    label: "Claude Code",
    binary: "claude",
    config_dir: ".claude",
    agent_rules_filename: "CLAUDE.md",
    initial_prompt_filename: "initial-prompt.txt",
};

/// Reference implementation of [`AgentDriver`] for Claude Code.
///
/// Declares all capabilities (Claude is the full-fidelity reference driver).
/// Behavioural methods delegate to existing engine code and will be extracted
/// from [`crate::effort`], [`crate::worker_setup`], [`crate::runner`], and
/// [`crate::transient_error`] in subsequent tasks.
pub struct ClaudeDriver;

#[async_trait]
impl AgentDriver for ClaudeDriver {
    fn descriptor(&self) -> &DriverDescriptor {
        &CLAUDE_DESCRIPTOR
    }

    fn capabilities(&self) -> CapabilitySet {
        // Claude provides all capabilities. ToolProvisioning is declared
        // provided even though it is unused in v1 — the driver could in
        // principle inject MCP servers; it currently does not.
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
            Capability::ToolProvisioning,
            Capability::PromptComposition,
        ])
    }

    fn spawn_invocation(
        &self,
        model: &str,
        effort: Option<&str>,
        settings_path: Option<&Path>,
        non_opus_auto_mode: bool,
    ) -> String {
        let mut cmd = format!("claude --model {model}");
        if let Some(e) = effort {
            cmd.push_str(" --effort ");
            cmd.push_str(e);
        }
        if crate::effort::model_requires_auto_permissions(model) || non_opus_auto_mode {
            cmd.push_str(" --permission-mode auto");
        } else {
            cmd.push_str(" --dangerously-skip-permissions");
        }
        if let Some(settings) = settings_path {
            // Single-quote the path so a `$TMPDIR` with spaces survives
            // the pane's shell. Worker settings paths never contain a
            // single quote, so naive single-quoting is sufficient.
            cmd.push_str(&format!(" --settings '{}'", settings.display()));
        }
        cmd.push_str(" \"$(cat .claude/initial-prompt.txt)\"\n");
        cmd
    }

    async fn provision_workspace(&self, _workspace: &Path, _prompt_text: &str, _run_id: &str) -> anyhow::Result<()> {
        // TODO(@brianduff,2026-12-31): extract from worker_setup::write_workspace_files
        unimplemented!("extracted in the WorkspaceProvisioning task")
    }

    async fn write_permission_config(
        &self,
        input: &PermissionInput,
        dest_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        use crate::worker_setup::{
            WorkerSetupInput, ensure_path_guard_script_in, render_remote_settings_json_for_dir,
            render_settings_json_for_dir,
        };

        std::fs::create_dir_all(dest_dir)?;

        // Write the path-guard script next to the settings file so the
        // PreToolUse hook command (baked into the settings JSON by absolute
        // path) resolves correctly when Claude executes it.
        ensure_path_guard_script_in(dest_dir)?;

        let setup = WorkerSetupInput {
            run_id: input.run_id.clone(),
            lease_id: input.lease_id.clone(),
            workspace_path: input.workspace_path.clone(),
            events_socket_path: input.events_socket_path.clone(),
            boss_event_path: input.boss_event_path.clone(),
            // draft_pr_mode is a CLAUDE.md concern, not a settings concern.
            draft_pr_mode: false,
            execution_kind: input.execution_kind.clone(),
            task_kind: input.task_kind.clone(),
            worker_kind: input.worker_kind.clone(),
        };

        let json = if input.is_remote {
            render_remote_settings_json_for_dir(&setup, dest_dir)
        } else {
            render_settings_json_for_dir(&setup, dest_dir)
        };

        let settings_path = dest_dir.join("settings.json");
        std::fs::write(&settings_path, json)?;
        Ok(settings_path)
    }

    fn agent_rules_preamble(&self) -> &'static str {
        // TODO(@brianduff,2026-12-31): extract from worker_setup::render_claude_md
        unimplemented!("extracted in the PromptComposition task")
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        // TODO(@brianduff,2026-12-31): extract from transient_error::classify_claude_error
        unimplemented!("extracted in the ControlVerbs task")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::Capability;

    #[test]
    fn claude_driver_provides_all_capabilities() {
        let driver = ClaudeDriver;
        let caps = driver.capabilities();

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
            Capability::ToolProvisioning,
            Capability::PromptComposition,
        ] {
            assert!(caps.provides(cap), "ClaudeDriver must provide {cap:?}",);
        }
    }

    #[test]
    fn claude_descriptor_slug_is_claude() {
        let driver = ClaudeDriver;
        assert_eq!(driver.descriptor().name, "claude");
        assert_eq!(driver.descriptor().config_dir, ".claude");
        assert_eq!(driver.descriptor().agent_rules_filename, "CLAUDE.md");
        assert_eq!(driver.descriptor().binary, "claude");
    }

    fn sample_permission_input(workspace: &std::path::Path) -> PermissionInput {
        PermissionInput {
            worker_kind: crate::worker_setup::WorkerKind::Standard,
            workspace_path: workspace.to_path_buf(),
            events_socket_path: std::path::PathBuf::from(
                "/Users/test/Library/Application Support/Boss/events.sock",
            ),
            boss_event_path: std::path::PathBuf::from("/usr/local/bin/boss-event"),
            run_id: "run-driver-test".to_owned(),
            lease_id: "lease-driver-test".to_owned(),
            execution_kind: "chore_implementation".to_owned(),
            task_kind: Some("chore".to_owned()),
            is_remote: false,
        }
    }

    #[tokio::test]
    async fn write_permission_config_creates_settings_json() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        let settings_path = driver
            .write_permission_config(&sample_permission_input(workspace.path()), dest.path())
            .await
            .unwrap();

        assert!(settings_path.exists(), "settings.json must exist");
        assert_eq!(settings_path, dest.path().join("settings.json"));

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(
            parsed["permissions"]["defaultMode"],
            serde_json::Value::String("auto".into()),
            "settings must pin defaultMode to auto",
        );
    }

    #[tokio::test]
    async fn write_permission_config_wires_all_seven_hook_events() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        let settings_path = driver
            .write_permission_config(&sample_permission_input(workspace.path()), dest.path())
            .await
            .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let hooks = parsed.get("hooks").unwrap().as_object().unwrap();
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "Notification",
            "SessionEnd",
        ] {
            assert!(hooks.contains_key(event), "missing hook event: {event}");
        }
    }

    #[tokio::test]
    async fn write_permission_config_denies_bossctl_and_engine_lifecycle_verbs() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        let settings_path = driver
            .write_permission_config(&sample_permission_input(workspace.path()), dest.path())
            .await
            .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let deny: Vec<&str> = parsed["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for rule in [
            "Bash(bossctl)",
            "Bash(bossctl:*)",
            "Bash(boss engine start)",
            "Bash(boss engine stop)",
        ] {
            assert!(
                deny.contains(&rule),
                "settings must deny {rule} (got {deny:?})",
            );
        }
    }

    #[tokio::test]
    async fn write_permission_config_writes_path_guard_script_to_dest_dir() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        driver
            .write_permission_config(&sample_permission_input(workspace.path()), dest.path())
            .await
            .unwrap();

        let guard_script = dest.path().join("boss-path-guard.py");
        assert!(
            guard_script.exists(),
            "path-guard script must be written to dest_dir",
        );
    }

    #[tokio::test]
    async fn write_permission_config_path_guard_hook_references_dest_dir_script() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        let settings_path = driver
            .write_permission_config(&sample_permission_input(workspace.path()), dest.path())
            .await
            .unwrap();

        let content = std::fs::read_to_string(&settings_path).unwrap();
        // The guard hook command must reference the script inside dest_dir,
        // not some other fixed location.
        let expected_script = dest.path().join("boss-path-guard.py");
        assert!(
            content.contains(expected_script.to_str().unwrap()),
            "settings JSON must reference the guard script in dest_dir: {}",
            expected_script.display(),
        );
    }

    #[tokio::test]
    async fn write_permission_config_remote_worker_omits_sandbox() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        let mut input = sample_permission_input(workspace.path());
        input.is_remote = true;
        input.events_socket_path =
            std::path::PathBuf::from("/tmp/boss-events-run-driver-test.sock");

        let settings_path = driver
            .write_permission_config(&input, dest.path())
            .await
            .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let deny: Vec<&str> = parsed["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        // Remote workers must NOT deny /tmp (the forwarded socket parent).
        assert!(
            !deny.iter().any(|r| r.contains("/tmp")),
            "remote settings must not deny /tmp (got {deny:?})",
        );
        // Static guards still apply.
        assert!(
            deny.contains(&"Bash(bossctl)"),
            "bossctl deny must remain for remote workers",
        );
    }

    #[tokio::test]
    async fn write_permission_config_reviewer_worker_adds_no_publish_rules() {
        let workspace = tempfile::TempDir::new().unwrap();
        let dest = tempfile::TempDir::new().unwrap();
        let driver = ClaudeDriver;

        let mut input = sample_permission_input(workspace.path());
        input.worker_kind = crate::worker_setup::WorkerKind::Reviewer;

        let settings_path = driver
            .write_permission_config(&input, dest.path())
            .await
            .unwrap();

        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let deny: Vec<&str> = parsed["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for rule in ["Bash(jj git push:*)", "Bash(gh pr create:*)", "Bash(cube pr:*)"] {
            assert!(
                deny.contains(&rule),
                "reviewer settings must deny {rule} (got {deny:?})",
            );
        }
    }
}
