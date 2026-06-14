//! `ClaudeDriver` — the reference implementation of [`AgentDriver`] for
//! Claude Code. The `Spawn`, `WorkspaceProvisioning`, and `PromptComposition`
//! capabilities are live; remaining behavioural methods are `unimplemented!()`
//! pending their per-capability extraction tasks (Depth 1–2 in the design).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use async_trait::async_trait;

use super::{AgentDriver, Capability, CapabilitySet, DriverDescriptor, WorkerErrorClass};

static CLAUDE_DESCRIPTOR: DriverDescriptor = DriverDescriptor {
    name: "claude",
    label: "Claude Code",
    binary: "claude",
    config_dir: ".claude",
    agent_rules_filename: "CLAUDE.md",
    initial_prompt_filename: "initial-prompt.txt",
};

/// The driver-specific preamble for the agent-rules file. Names the hook
/// mechanism ("claude hooks") and is injected at the top of `CLAUDE.md` by
/// [`render_claude_md`][crate::worker_setup::render_claude_md].
const CLAUDE_AGENT_RULES_PREAMBLE: &str =
    "You are running inside a Boss-managed worker session. The engine\n\
     spawned you in a leased cube workspace and observes this session\n\
     via claude hooks.";

/// Reference implementation of [`AgentDriver`] for Claude Code.
///
/// Declares all capabilities (Claude is the full-fidelity reference driver).
/// Behavioural methods are extracted from [`crate::effort`],
/// [`crate::worker_setup`], [`crate::runner`], and [`crate::transient_error`].
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
        // Use the descriptor's config_dir and initial_prompt_filename so this
        // stays in sync with provision_workspace's write location.
        cmd.push_str(&format!(
            " \"$(cat {}/{})\"\n",
            CLAUDE_DESCRIPTOR.config_dir,
            CLAUDE_DESCRIPTOR.initial_prompt_filename,
        ));
        cmd
    }

    /// Write per-session workspace files and suppress the first-run trust
    /// prompt. Specifically:
    ///
    /// - Creates `<workspace>/<config_dir>/` (`.claude/`)
    /// - Writes the initial prompt to `<config_dir>/<initial_prompt_filename>`
    /// - Writes a catch-all `.gitignore` so engine-injected files never appear
    ///   in `jj status` / `git status`
    /// - Pre-seeds `~/.claude.json` so the folder-trust dialog does not block
    ///   the headless worker session
    async fn provision_workspace(
        &self,
        workspace: &Path,
        prompt_text: &str,
        _run_id: &str,
    ) -> anyhow::Result<()> {
        let config_dir = workspace.join(CLAUDE_DESCRIPTOR.config_dir);
        std::fs::create_dir_all(&config_dir)
            .with_context(|| format!("creating {}", config_dir.display()))?;

        let prompt_path = config_dir.join(CLAUDE_DESCRIPTOR.initial_prompt_filename);
        std::fs::write(&prompt_path, prompt_text)
            .with_context(|| format!("writing initial prompt to {}", prompt_path.display()))?;

        let gitignore_path = config_dir.join(".gitignore");
        std::fs::write(&gitignore_path, crate::worker_setup::CLAUDE_DIR_GITIGNORE)
            .with_context(|| format!("writing gitignore to {}", gitignore_path.display()))?;

        // Pre-seed the Claude global config so the folder-trust dialog does
        // not block the headless worker. Best-effort: failure is logged and
        // swallowed by pre_trust_workspace.
        crate::worker_setup::pre_trust_workspace(workspace);

        Ok(())
    }

    async fn write_permission_config(&self, _dest_dir: &Path) -> anyhow::Result<PathBuf> {
        // TODO(@brianduff,2026-12-31): extract from worker_setup::render_settings_json
        unimplemented!("extracted in the PermissionPolicy task")
    }

    /// The Claude-specific preamble injected at the top of `CLAUDE.md`.
    /// Names "claude hooks" as the observability mechanism and is distinct
    /// from the driver-agnostic body that follows it.
    fn agent_rules_preamble(&self) -> &'static str {
        CLAUDE_AGENT_RULES_PREAMBLE
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
    use tempfile::TempDir;

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

    #[test]
    fn agent_rules_preamble_names_claude_hooks() {
        let preamble = ClaudeDriver.agent_rules_preamble();
        assert!(preamble.contains("claude hooks"), "preamble must name 'claude hooks': {preamble}");
        assert!(preamble.contains("Boss-managed"), "preamble must describe Boss session: {preamble}");
    }

    #[tokio::test]
    async fn provision_workspace_writes_prompt_gitignore_and_pretrust() {
        use std::sync::Mutex;
        // HOME must be redirected so pre_trust_workspace doesn't write to the
        // developer's real ~/.claude.json.
        static HOME_LOCK: Mutex<()> = Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let workspace = TempDir::new().unwrap();
        let fake_home = TempDir::new().unwrap();
        let original_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home.path()); }

        let driver = ClaudeDriver;
        driver.provision_workspace(workspace.path(), "hello prompt", "run-1")
            .await
            .unwrap();

        // Prompt file at the descriptor-derived path.
        let prompt_path = workspace.path().join(".claude").join("initial-prompt.txt");
        assert!(prompt_path.exists(), "prompt file must exist at {}", prompt_path.display());
        assert_eq!(std::fs::read_to_string(&prompt_path).unwrap(), "hello prompt");

        // Gitignore must exist and catch all files.
        let gitignore_path = workspace.path().join(".claude").join(".gitignore");
        assert!(gitignore_path.exists(), ".gitignore must exist");
        assert_eq!(std::fs::read_to_string(&gitignore_path).unwrap(), "*\n");

        // Pre-trust must have seeded ~/.claude.json.
        let claude_json = fake_home.path().join(".claude.json");
        assert!(claude_json.exists(), "~/.claude.json must have been written");
        let val: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&claude_json).unwrap()
        ).unwrap();
        let key = workspace.path().display().to_string();
        assert_eq!(val["projects"][&key]["hasTrustDialogAccepted"], true);

        // Restore HOME.
        match original_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn spawn_invocation_uses_descriptor_paths() {
        let cmd = ClaudeDriver.spawn_invocation("sonnet", None, None, false);
        let expected_cat = format!(
            "\"$(cat {}/{})\"\n",
            CLAUDE_DESCRIPTOR.config_dir,
            CLAUDE_DESCRIPTOR.initial_prompt_filename,
        );
        assert!(
            cmd.contains(&expected_cat),
            "spawn invocation must read from descriptor paths; got: {cmd}",
        );
    }
}
