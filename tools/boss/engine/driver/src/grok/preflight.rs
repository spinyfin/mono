//! Fail-fast capability checks for the scoped Grok worker environment.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use super::environment::GrokProcessEnvironment;

struct PreflightOutput {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

trait PreflightRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        workspace: &Path,
        environment: &GrokProcessEnvironment,
    ) -> anyhow::Result<PreflightOutput>;
}

struct RealPreflightRunner {
    macos_seatbelt_profile: Option<PathBuf>,
}

impl PreflightRunner for RealPreflightRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        workspace: &Path,
        environment: &GrokProcessEnvironment,
    ) -> anyhow::Result<PreflightOutput> {
        let mut command = match &self.macos_seatbelt_profile {
            Some(profile) => {
                let mut command = Command::new("/usr/bin/sandbox-exec");
                command.arg("-f").arg(profile).arg(program);
                command
            }
            None => Command::new(program),
        };
        command.args(args).current_dir(workspace);
        if program == "grok" {
            environment.apply_to_command(&mut command);
        } else {
            environment.apply_tool_sandbox_environment(&mut command);
        }
        let output = command
            .output()
            .with_context(|| format!("starting Grok worker preflight capability `{program}`"))?;
        Ok(PreflightOutput {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Prove that every capability the worker needs is usable before the pane is
/// spawned. Each check requires affirmative output in addition to exit zero;
/// this matters because `grok models` returns zero while printing "not
/// authenticated" when no credential is usable.
pub fn run_worker_preflight(workspace: &Path, environment: &GrokProcessEnvironment) -> anyhow::Result<()> {
    run_worker_preflight_with(
        &RealPreflightRunner {
            macos_seatbelt_profile: None,
        },
        workspace,
        environment,
    )
}

/// Run the same affirmative checks under the exact Boss-owned Seatbelt
/// profile that will contain the Grok pane and all of its terminal tools.
/// A direct parent-process check is insufficient: Grok's built-in macOS
/// profile can make `gh auth status` degrade from keyring to a stale file
/// credential even though the same scoped HOME succeeds outside Seatbelt.
pub fn run_worker_preflight_under_macos_seatbelt(
    workspace: &Path,
    environment: &GrokProcessEnvironment,
    profile: &Path,
) -> anyhow::Result<()> {
    run_worker_preflight_with(
        &RealPreflightRunner {
            macos_seatbelt_profile: Some(profile.to_path_buf()),
        },
        workspace,
        environment,
    )
}

fn run_worker_preflight_with(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
) -> anyhow::Result<()> {
    let grok = runner.run("grok", &["models"], workspace, environment)?;
    assert_grok_oauth(&grok)?;

    let workspace_arg = workspace.display().to_string();
    let cube = runner.run(
        "cube",
        &["--json", "workspace", "status", "--workspace", &workspace_arg],
        workspace,
        environment,
    )?;
    assert_cube_workspace(&cube, workspace)?;

    let gh = runner.run(
        "gh",
        &["auth", "status", "--active", "--hostname", "github.com"],
        workspace,
        environment,
    )?;
    assert_gh_keyring(&gh)?;

    let jj_root = runner.run("jj", &["root"], workspace, environment)?;
    assert_jj_root(&jj_root, workspace)?;

    let jj_remotes = runner.run("jj", &["git", "remote", "list"], workspace, environment)?;
    assert_jj_remotes(&jj_remotes)?;

    Ok(())
}

fn assert_grok_oauth(output: &PreflightOutput) -> anyhow::Result<()> {
    require_success("Grok OAuth", output)?;
    if !output.stdout.contains("You are logged in with grok.com.") {
        bail!(
            "Grok worker preflight failed: Grok OAuth is unavailable; `grok models` did not affirm a grok.com login. {}",
            rendered_output(output)
        );
    }
    Ok(())
}

fn assert_cube_workspace(output: &PreflightOutput, workspace: &Path) -> anyhow::Result<()> {
    require_success("Cube workspace access", output)?;
    let value: serde_json::Value = serde_json::from_str(&output.stdout).with_context(|| {
        format!(
            "Grok worker preflight failed: Cube workspace access returned non-JSON output. {}",
            rendered_output(output)
        )
    })?;
    let reported = value
        .pointer("/workspace/workspace_path")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .context("Grok worker preflight failed: Cube workspace access returned no workspace.workspace_path")?;
    if !same_path(&reported, workspace) {
        bail!(
            "Grok worker preflight failed: Cube resolved workspace {} instead of {}",
            reported.display(),
            workspace.display()
        );
    }
    Ok(())
}

fn assert_gh_keyring(output: &PreflightOutput) -> anyhow::Result<()> {
    require_success("gh authentication", output)?;
    let combined = format!("{}\n{}", output.stdout, output.stderr);
    if !combined.contains("Logged in to github.com") || !combined.contains("(keyring)") {
        bail!(
            "Grok worker preflight failed: gh authentication did not resolve a github.com keyring credential. {}",
            rendered_output(output)
        );
    }
    Ok(())
}

fn assert_jj_root(output: &PreflightOutput, workspace: &Path) -> anyhow::Result<()> {
    require_success("jj workspace access", output)?;
    let reported = PathBuf::from(output.stdout.trim());
    if reported.as_os_str().is_empty() || !same_path(&reported, workspace) {
        bail!(
            "Grok worker preflight failed: jj workspace access resolved {} instead of {}",
            reported.display(),
            workspace.display()
        );
    }
    Ok(())
}

fn assert_jj_remotes(output: &PreflightOutput) -> anyhow::Result<()> {
    require_success("jj/git remote access", output)?;
    if !output.stdout.contains("github.com") {
        bail!(
            "Grok worker preflight failed: jj/git remote access found no github.com remote. {}",
            rendered_output(output)
        );
    }
    Ok(())
}

fn require_success(capability: &str, output: &PreflightOutput) -> anyhow::Result<()> {
    if !output.success {
        bail!(
            "Grok worker preflight failed: {capability} command exited {}. {}",
            output.status,
            rendered_output(output)
        );
    }
    Ok(())
}

fn rendered_output(output: &PreflightOutput) -> String {
    format!("stdout={:?} stderr={:?}", output.stdout.trim(), output.stderr.trim())
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct FakeRunner {
        outputs: RefCell<VecDeque<PreflightOutput>>,
        programs: RefCell<Vec<String>>,
    }

    impl FakeRunner {
        fn new(outputs: Vec<PreflightOutput>) -> Self {
            Self {
                outputs: RefCell::new(outputs.into()),
                programs: RefCell::new(Vec::new()),
            }
        }
    }

    impl PreflightRunner for FakeRunner {
        fn run(
            &self,
            program: &str,
            _args: &[&str],
            _workspace: &Path,
            _environment: &GrokProcessEnvironment,
        ) -> anyhow::Result<PreflightOutput> {
            self.programs.borrow_mut().push(program.to_owned());
            self.outputs
                .borrow_mut()
                .pop_front()
                .with_context(|| format!("no fake output for {program}"))
        }
    }

    fn success(stdout: impl Into<String>) -> PreflightOutput {
        PreflightOutput {
            success: true,
            status: "exit status: 0".to_owned(),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn environment() -> GrokProcessEnvironment {
        GrokProcessEnvironment::for_test()
    }

    #[test]
    fn preflight_runs_every_required_capability() {
        let workspace = Path::new("/workspace");
        let runner = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            success(r#"{"workspace":{"workspace_path":"/workspace"}}"#),
            success("Logged in to github.com account worker (keyring)\n"),
            success("/workspace\n"),
            success("origin git@github.com:example/repo.git\n"),
        ]);

        run_worker_preflight_with(&runner, workspace, &environment()).unwrap();
        assert_eq!(runner.programs.into_inner(), ["grok", "cube", "gh", "jj", "jj"]);
    }

    #[test]
    fn grok_models_silent_success_is_rejected() {
        let output = success("You are not authenticated.\nDefault model: grok-4.5\n");
        let error = assert_grok_oauth(&output).unwrap_err().to_string();
        assert!(error.contains("Grok OAuth is unavailable"), "{error}");
    }

    #[test]
    fn gh_without_keyring_is_rejected() {
        let output = success("Logged in to github.com account worker (default)\n");
        let error = assert_gh_keyring(&output).unwrap_err().to_string();
        assert!(error.contains("keyring credential"), "{error}");
    }

    #[test]
    fn cube_wrong_workspace_is_rejected() {
        let output = success(r#"{"workspace":{"workspace_path":"/other"}}"#);
        let error = assert_cube_workspace(&output, Path::new("/workspace"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("instead of /workspace"), "{error}");
    }
}
