//! `ClaudeDriver` stub — the reference implementation of [`AgentDriver`] for
//! Claude Code. Behavioural methods are `unimplemented!()` pending the
//! per-capability extraction tasks (Depth 1–2 in the design).

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use super::{
    AgentDriver, Capability, CapabilitySet, DriverDescriptor, WorkerErrorClass,
};

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
        _model: &str,
        _effort: Option<&str>,
        _settings_path: Option<&Path>,
        _non_opus_auto_mode: bool,
    ) -> String {
        // TODO(driver-spawn): extract from effort::SpawnConfig::claude_invocation
        unimplemented!("extracted in the Spawn capability task")
    }

    async fn provision_workspace(
        &self,
        _workspace: &Path,
        _prompt_text: &str,
        _run_id: &str,
    ) -> anyhow::Result<()> {
        // TODO(driver-workspace): extract from worker_setup::write_workspace_files
        unimplemented!("extracted in the WorkspaceProvisioning task")
    }

    async fn write_permission_config(&self, _dest_dir: &Path) -> anyhow::Result<PathBuf> {
        // TODO(driver-permission): extract from worker_setup::render_settings_json
        unimplemented!("extracted in the PermissionPolicy task")
    }

    fn agent_rules_preamble(&self) -> &'static str {
        // TODO(driver-prompt): extract from worker_setup::render_claude_md
        unimplemented!("extracted in the PromptComposition task")
    }

    fn classify_error(&self, _raw_output: &str) -> WorkerErrorClass {
        // TODO(driver-control): extract from transient_error::classify_claude_error
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
            assert!(
                caps.provides(cap),
                "ClaudeDriver must provide {cap:?}",
            );
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
}
