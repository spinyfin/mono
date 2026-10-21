//! Private initial-input artifacts and the interactive terminal launcher.
//!
//! Prompt bytes never enter a shell command or an environment variable. The
//! launcher owns delivery, so an engine restart cannot interrupt or repeat it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

pub const LAUNCHER: &str = include_str!("initial_prompt.py");

/// Shared by local provisioning and SSH staging.
pub fn launcher_command(config_dir: &str, command: &str) -> String {
    format!("python3 {config_dir}/initial-prompt.py {}", command.trim_start())
}

pub fn launcher_config(driver: &dyn crate::AgentDriver) -> Result<String> {
    let spec = driver
        .pane_monitor_spec()
        .context("driver has no initial-input readiness markers")?;
    anyhow::ensure!(!spec.agent_markers.is_empty(), "driver has no readiness markers");
    let mut starting_markers = spec.starting_markers;
    starting_markers.extend(
        [
            "Choose the text style",
            "Sign in with",
            "Sign in to",
            "Waiting for approval",
            "Connecting...",
        ]
        .map(String::from),
    );
    Ok(serde_json::to_string(&serde_json::json!({
        "agent_markers": spec.agent_markers,
        "starting_markers": starting_markers,
    }))?)
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("opening private worker input {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    Ok(())
}

/// The config directory belongs to this leased worker workspace. Drivers
/// with retained per-run homes keep the actual brief there; the workspace
/// reference holds no prompt bytes. Claude records a private input home under
/// the same engine-owned homes root for the restart-safe retention sweep.
pub fn provision(config_dir: &Path, run_home: &Path, prompt: &str, driver: &dyn crate::AgentDriver) -> Result<()> {
    let preflight = boss_command_runner::output_blocking_timeout(
        std::process::Command::new("python3").args(["-c", "import fcntl, pty, termios"]),
        std::time::Duration::from_secs(5),
    )
    .context("python3 with Unix PTY support is required for private initial brief delivery")?;
    anyhow::ensure!(
        preflight.status.success(),
        "python3 initial brief preflight failed: {}",
        String::from_utf8_lossy(&preflight.stderr)
    );
    fs::create_dir_all(run_home)?;
    let prompt_path = run_home.join("initial-prompt.txt");
    write_private(&prompt_path, prompt.as_bytes())?;
    let reference = config_dir.join("initial-prompt.txt");
    if reference != prompt_path {
        match fs::remove_file(&reference) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::os::unix::fs::symlink(&prompt_path, &reference)?;
    }
    write_private(
        &config_dir.join("initial-prompt.json"),
        launcher_config(driver)?.as_bytes(),
    )?;
    write_private(&config_dir.join("initial-prompt.py"), LAUNCHER.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_relay_preserves_followups_resize_and_exit_status() {
        let temp = tempfile::tempdir().unwrap();
        provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
        fs::write(
            temp.path().join("panes.json"),
            include_str!("testdata/initial_prompt_panes.json"),
        )
        .unwrap();
        let configs: serde_json::Map<String, serde_json::Value> = ["claude", "codex", "grok"]
            .into_iter()
            .map(|name| {
                let driver = crate::DriverRegistry::default().require(name).unwrap();
                (
                    name.to_owned(),
                    serde_json::from_str(&launcher_config(driver.as_ref()).unwrap()).unwrap(),
                )
            })
            .collect();
        fs::write(
            temp.path().join("configs.json"),
            serde_json::to_string(&configs).unwrap(),
        )
        .unwrap();
        let harness = temp.path().join("relay-test.py");
        fs::write(&harness, include_str!("initial_prompt_relay_test.py")).unwrap();
        let trace = temp.path().join("relay-trace.txt");
        let output = boss_command_runner::output_blocking_timeout_in_group(
            std::process::Command::new("python3")
                .arg(harness)
                .arg(temp.path())
                .env("BOSS_RELAY_TRACE", &trace),
            std::time::Duration::from_secs(45),
            |_| {},
        )
        .unwrap_or_else(|error| panic!("{error}: {}", fs::read_to_string(trace).unwrap_or_default()));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn private_brief_is_in_run_home_and_reprovision_corrects_permissions() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let home = temp.path().join("run-home");
        fs::create_dir(&config).unwrap();
        provision(&config, &home, "private sentinel\n\n", &crate::ClaudeDriver).unwrap();
        let path = home.join("initial-prompt.txt");
        assert_eq!(fs::read_link(config.join("initial-prompt.txt")).unwrap(), path);
        assert_eq!(fs::read_to_string(&path).unwrap(), "private sentinel\n\n");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        provision(&config, &home, "replacement", &crate::ClaudeDriver).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "replacement");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn unreadable_or_nonprivate_brief_fails_before_driver_exec() {
        for missing in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
            let prompt = temp.path().join("initial-prompt.txt");
            if missing {
                fs::remove_file(&prompt).unwrap();
            } else {
                fs::set_permissions(&prompt, fs::Permissions::from_mode(0o644)).unwrap();
            }
            let output = std::process::Command::new("python3")
                .arg(temp.path().join("initial-prompt.py"))
                .args(["python3", "-c", "print('DRIVER_EXECUTED')"])
                .output()
                .unwrap();
            assert!(!output.status.success());
            assert!(!String::from_utf8_lossy(&output.stdout).contains("DRIVER_EXECUTED"));
            assert!(String::from_utf8_lossy(&output.stderr).contains("boss initial brief delivery failed"));
        }
    }

    #[test]
    fn early_driver_exit_is_a_launch_failure() {
        let temp = tempfile::tempdir().unwrap();
        provision(temp.path(), temp.path(), "private sentinel", &crate::ClaudeDriver).unwrap();
        let output = std::process::Command::new("python3")
            .arg(temp.path().join("initial-prompt.py"))
            .args(["python3", "-c", "print('before readiness')"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("before receiving the initial brief"));
    }
}
