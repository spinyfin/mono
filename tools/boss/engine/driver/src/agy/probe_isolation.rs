//! Shared isolation setup for standalone agy evidence binaries.
//!
//! These binaries run before any threads start and redirect HOME-derived
//! paths into a caller-selected evidence directory so a diagnostic never
//! writes into the developer's live Boss state.

use std::fs::DirBuilder;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;

use anyhow::Context;
use boss_ssh_transport::shell_quote;

use super::environment::AGY_SKIP_BINARY_ASSERT_ENV;
use super::home::{AGY_HOMES_ROOT_ENV, AGY_SKIP_POSTURE_ASSERT_ENV, AgyRuntimeState};
use super::permissions;
use crate::WorkerKind;
use crate::transcript_store::WORKER_TRANSCRIPTS_ROOT_ENV;

/// Create `directory` mode 0700 and point every agy home / transcript override
/// at children of it. Also clears the test-only skip flags so a live probe
/// cannot inherit a loosened posture from the parent environment.
///
/// # Safety
///
/// Mutates process environment. Call only from a standalone binary `main`
/// before any threads exist.
pub unsafe fn prepare_evidence_dir(directory: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(directory.is_absolute(), "evidence directory must be absolute");
    DirBuilder::new().mode(0o700).create(directory)?;
    // This standalone process has no threads yet; never mutate engine globals.
    unsafe {
        std::env::set_var(AGY_HOMES_ROOT_ENV, directory.join("homes"));
        // Provisioning of worker homes links transcripts into the durable store;
        // keep the probe's inside its own evidence directory, never Boss's.
        std::env::set_var(WORKER_TRANSCRIPTS_ROOT_ENV, directory.join("transcripts"));
        std::env::remove_var(AGY_SKIP_BINARY_ASSERT_ENV);
        std::env::remove_var(AGY_SKIP_POSTURE_ASSERT_ENV);
    }
    Ok(())
}

/// Live-validated PreToolUse deny: record the hook payload, emit a deny
/// decision, and exit 2. `/bin/cat` drains stdin so the CLI's hook body is
/// consumed; exit 2 is the fail-closed status the rules probe measured.
pub(crate) fn deny_all_pre_tool_use_command(attempts: &Path) -> String {
    format!(
        "/usr/bin/printf attempt >> {0} || exit 2; /bin/cat >> {0} || exit 2; /usr/bin/printf '%s\\n' '{{\"decision\":\"deny\",\"reason\":\"probe forbids tools\"}}'; exit 2",
        shell_quote(&attempts.display().to_string())
    )
}

fn deny_all_hooks_json(session_start_command: &str, attempts: &Path) -> String {
    serde_json::to_string_pretty(&serde_json::json!({"boss": {
        "SessionStart": [{"type": "command", "command": session_start_command}],
        "PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command":
            deny_all_pre_tool_use_command(attempts)
        }]}]
    }}))
    .expect("deny-all hooks are serializable")
        + "\n"
}

/// Render Standard-worker session settings and overwrite the run's
/// `hooks.json` with a deny-all PreToolUse hook. `settings_path` is the
/// `--settings` file the probe will pass; `session_start_command` is the
/// SessionStart handler (`true` for quota, a liveness touch for the rules
/// probe); `tool_attempts` is the file the deny command appends to.
pub(crate) fn install_standard_fail_closed_posture(
    runtime: &AgyRuntimeState,
    workspace: &Path,
    settings_path: &Path,
    session_start_command: &str,
    tool_attempts: &Path,
) -> anyhow::Result<()> {
    let workspace_abs = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let hooks_dir = runtime.process_home.join(".gemini/config");
    let rules_root = runtime
        .process_home
        .parent()
        .context("probe container")?
        .join("rules-root");
    let settings = permissions::render_settings(
        &permissions::SettingsPlan::builder()
            .worker_kind(WorkerKind::Standard)
            .workspace(&workspace_abs)
            .hooks_dir(&hooks_dir)
            .control_dir(&runtime.control_dir)
            .rules_root(&rules_root)
            .build(),
    )?;
    std::fs::write(settings_path, settings.to_json()?)
        .with_context(|| format!("writing session settings {}", settings_path.display()))?;
    std::fs::write(
        hooks_dir.join("hooks.json"),
        deny_all_hooks_json(session_start_command, tool_attempts),
    )
    .with_context(|| format!("writing deny-all hooks under {}", hooks_dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn evidence_directory_must_be_absolute() {
        // SAFETY: the relative-path check returns before any env mutation.
        let err = unsafe { prepare_evidence_dir(Path::new("relative")) }.unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
    }

    #[test]
    fn deny_hook_records_attempt_and_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let attempts = tmp.path().join("attempts with spaces");
        let mut child = Command::new("/bin/sh")
            .args(["-c", &deny_all_pre_tool_use_command(&attempts)])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"tool\":\"read_file\"}\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(
            std::fs::read_to_string(attempts).unwrap(),
            "attempt{\"tool\":\"read_file\"}\n"
        );
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["decision"], "deny");
    }
}
