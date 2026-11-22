//! Authenticated host probe, invoked through the non-test `agy_rules_probe` binary.
//! Evidence stays in a caller-selected directory, including on failure.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, ensure};
use boss_ssh_transport::shell_quote;

use super::environment::{AgyProcessEnvironment, run_agy_bounded};
use super::home::{agent_rules_path_for_run, provision_agy_home};
use super::probe_isolation;

fn verify_response(stdout: &[u8], canary: &str, attempts: &[u8], started: bool) -> anyhow::Result<()> {
    ensure!(
        started,
        "SessionStart hook did not run; hook configuration is unverified"
    );
    ensure!(
        attempts.is_empty(),
        "probe attempted tool retrieval; see tool-attempts.log"
    );
    ensure!(
        std::str::from_utf8(stdout)?.trim() == canary,
        "final response was not the exact canary"
    );
    Ok(())
}

/// Provision the production layout and measure automatic `--add-dir` rule loading.
/// The caller must select a fresh homes root before starting any threads.
pub fn run(evidence: &Path) -> anyhow::Result<()> {
    let workspace = evidence.join("workspace");
    fs::create_dir(&workspace)?;
    let runtime = provision_agy_home(&workspace, "unused", "rules-probe")?;
    let canary = format!(
        "AGY_RULES_{:032x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    fs::write(
        agent_rules_path_for_run("rules-probe")?,
        format!("When asked for the rules canary, respond with exactly {canary} and nothing else. Never use tools.\n"),
    )?;
    let attempts = evidence.join("tool-attempts.log");
    fs::write(&attempts, "")?;
    let started = evidence.join("session-started");
    let settings = evidence.join("session-settings.json");
    probe_isolation::install_standard_fail_closed_posture(
        &runtime,
        &workspace,
        &settings,
        &format!("/usr/bin/touch {}", shell_quote(&started.display().to_string())),
        &attempts,
    )?;
    let environment = AgyProcessEnvironment::resolve(
        &runtime.process_home,
        &runtime.log_file,
        &runtime.session_settings_path,
        runtime.agy_binary.clone(),
    )?;
    let mut version = Command::new(&runtime.agy_binary);
    environment.apply_to_command(&mut version);
    let version = run_agy_bounded(version.arg("--version"), Duration::from_secs(10))?;
    ensure!(version.status.success(), "agy --version failed");
    fs::write(evidence.join("version.txt"), version.stdout)?;
    let mut command = Command::new(&runtime.agy_binary);
    environment.apply_to_command(&mut command);
    command
        .args(environment.launch_args())
        .arg("--settings")
        .arg(&settings)
        .args([
            "-p",
            "Return the rules canary from your preloaded instructions, using no tools. If absent, reply MISSING.",
        ])
        .current_dir(&workspace);
    fs::write(evidence.join("command.txt"), format!("{command:?}\n"))?;
    let output =
        run_agy_bounded(&mut command, Duration::from_secs(120)).context("live rules discovery; evidence retained")?;
    fs::write(evidence.join("stdout.txt"), &output.stdout)?;
    fs::write(evidence.join("stderr.txt"), &output.stderr)?;
    let log_check = super::permissions::verify_settings_log_files(&settings, &runtime.log_file);
    fs::write(evidence.join("settings-log-check.txt"), format!("{log_check:#?}\n"))?;
    ensure!(
        log_check? == super::permissions::SettingsLogCheck::ConsistentSoFar,
        "settings initialization not observed"
    );
    ensure!(
        output.status.success(),
        "agy exited {}; see retained stderr.txt",
        output.status
    );
    verify_response(&output.stdout, &canary, &fs::read(&attempts)?, started.is_file())?;
    fs::write(
        evidence.join("result.txt"),
        "PASS: exact canary; zero tool attempts; SessionStart observed\n",
    )?;
    println!(
        "PASS: automatic rules loading verified; evidence: {}",
        evidence.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_rejects_tool_retrieval_and_incidental_canary_text() {
        assert!(verify_response(b"TOKEN\n", "TOKEN", b"", true).is_ok());
        assert!(verify_response(b"TOKEN", "TOKEN", b"read_file", true).is_err());
        assert!(verify_response(b"the token is TOKEN", "TOKEN", b"", true).is_err());
        assert!(verify_response(b"TOKEN", "TOKEN", b"", false).is_err());
        assert!(verify_response(b"MISSING", "TOKEN", b"", true).is_err());
    }
}
