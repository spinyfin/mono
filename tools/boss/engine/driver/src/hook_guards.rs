//! Shared canonical guard chain and executable installation for hook adapters.
use crate::ToolUseInterceptionConfig;
use crate::claude::{
    BOSS_LAUNCH_GUARD_COMMAND, REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND, REVISION_PR_GUARD_COMMAND,
    pr_redirect_guard_command,
};
use anyhow::Context;
use boss_ssh_transport::shell_quote;
use std::{fs, path::Path};
/// A canonical Boss guard command and its tool matcher, shared by adapters.
pub(crate) struct Guard {
    pub command: String,
    pub matcher: &'static str,
    /// Execution bound in seconds. Checkleft needs its longer build bound.
    pub timeout_sec: Option<u64>,
    /// Run once on the combined command forms instead of on each inspection pass.
    pub run_once: bool,
}

/// The five Boss `PreToolUse` guard commands, unchanged from Claude's. Order
/// matches `claude::ClaudeDriver::tool_use_interception_wiring` exactly.
/// Guards are omitted per the same rules Claude/Codex use (path guard +
/// checkleft guard need their materialised script path; PR-redirect +
/// checkleft are Standard-worker-only; revision guard is revision-only) —
/// the Boss-launch guard is the only one that is always present, so this
/// never returns empty as long as `config` reaches this function at all.
pub(crate) fn guard_commands(config: &ToolUseInterceptionConfig) -> Vec<Guard> {
    guard_commands_with_path_env(config, &[])
}

/// Same chain as [`guard_commands`], with extra `NAME=value` assignments on
/// the path-guard command itself. Callers that need an additional trusted
/// root (agy's conversation scratch) pass it here so it cannot land on a
/// different guard if the chain grows.
pub(crate) fn guard_commands_with_path_env(
    config: &ToolUseInterceptionConfig,
    path_env: &[(&str, &str)],
) -> Vec<Guard> {
    let mut out = Vec::new();

    // 1. Path guard (data-dir sandbox). Matcher `*`: it inspects
    //    file_path/notebook_path/path on every tool, not just Bash.
    if let (Some(data_dir), Some(guard_script)) = (&config.data_dir, &config.path_guard_script) {
        let mut assignments: Vec<String> = path_env
            .iter()
            .map(|(name, value)| format!("{name}={}", shell_quote(value)))
            .collect();
        assignments.push(format!(
            "BOSS_DATA_DIR={dir}",
            dir = shell_quote(&data_dir.display().to_string()),
        ));
        out.push(Guard {
            command: format!(
                "{env} python3 {script}",
                env = assignments.join(" "),
                script = shell_quote(&guard_script.display().to_string()),
            ),
            matcher: "*",
            timeout_sec: None,
            run_once: false,
        });
    }

    // 1b. Shared-source-store write gate (every local worker kind). Same
    //     script and hook-command shape as Claude; the adapter canonicalises
    //     Grok's payload into the Claude dialect the script reads.
    if let Some(guard_script) = &config.git_store_guard_script {
        out.push(Guard {
            command: crate::git_store_guard::hook_command(guard_script, config.workspace_path.as_deref()),
            matcher: "Bash",
            timeout_sec: None,
            run_once: false,
        });
    }

    // 2. Boss-launch guard. Always on, all workers.
    out.push(Guard {
        command: BOSS_LAUNCH_GUARD_COMMAND.to_owned(),
        matcher: "Bash",
        timeout_sec: None,
        run_once: false,
    });

    // 3. PR-redirect guard. Standard workers only (local + remote).
    if config.is_standard_worker {
        out.push(Guard {
            command: pr_redirect_guard_command(config.is_revision),
            matcher: "Bash",
            timeout_sec: None,
            run_once: false,
        });
    }

    // 4. Checkleft push guard. Local Standard workers only.
    if config.is_standard_worker
        && let Some(checkleft_script) = &config.checkleft_guard_script
    {
        out.push(Guard {
            command: format!(
                "python3 {script}",
                script = shell_quote(&checkleft_script.display().to_string())
            ),
            matcher: "Bash",
            timeout_sec: Some(crate::CHECKLEFT_GUARD_HOOK_TIMEOUT_SECS),
            run_once: true,
        });
    }

    // 5. Reviewer static-analysis guard. The shared Claude command is run by
    // Grok's adapter, so the three drivers enforce the same command policy.
    if config.is_reviewer {
        out.push(Guard {
            command: REVIEWER_STATIC_ANALYSIS_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
            timeout_sec: None,
            run_once: false,
        });
    }

    // 6. Revision PR guard.
    if config.is_revision {
        out.push(Guard {
            command: REVISION_PR_GUARD_COMMAND.to_owned(),
            matcher: "Bash",
            timeout_sec: None,
            run_once: false,
        });
    }

    if config.is_review_guide {
        out.push(Guard {
            command: format!(
                "python3 -c {}",
                shell_quote(&crate::codex::codex_review_guide_guard_script())
            ),
            matcher: ".*",
            timeout_sec: None,
            run_once: false,
        });
    }
    out
}

pub(crate) fn write_executable(path: &Path, body: &str) -> anyhow::Result<()> {
    fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).with_context(|| format!("chmod +x {}", path.display()))?;
    }
    Ok(())
}
