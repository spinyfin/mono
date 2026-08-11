//! Fail-fast capability checks for the scoped Grok worker environment.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use super::environment::GrokProcessEnvironment;

/// Wall-clock limit for the OAuth-sensitive `grok models` probe.
const GROK_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

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
        if program == "grok" {
            environment
                .wait_for_auth_ready_for_oauth_probe()
                .context("waiting for a Grok OAuth refresh before preflight")?;
        }
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
        let output = if program == "grok" {
            run_grok_models_with_timeout(&mut command, GROK_MODELS_TIMEOUT)?
        } else {
            command
                .output()
                .with_context(|| format!("starting Grok worker preflight capability `{program}`"))?
        };
        Ok(PreflightOutput {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Run `command` with a wall-clock deadline, capturing stdout/stderr.
///
/// `Command::spawn` inherits stdio by default; pipes must be set explicitly so
/// the OAuth affirmation in stdout is available to the caller. Pipes are drained
/// on dedicated threads while the parent only polls exit status — otherwise a
/// child that fills the ~64KiB pipe buffer deadlocks against a non-reading wait.
fn run_grok_models_with_timeout(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .context("starting Grok worker preflight capability `grok`")?;
    let stdout = child
        .stdout
        .take()
        .context("missing stdout pipe for Grok worker preflight capability `grok`")?;
    let stderr = child
        .stderr
        .take()
        .context("missing stderr pipe for Grok worker preflight capability `grok`")?;
    let stdout_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = stdout;
        reader.read_to_end(&mut buf).map(|_| buf)
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut reader = stderr;
        reader.read_to_end(&mut buf).map(|_| buf)
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .context("waiting for Grok worker preflight capability `grok`")?
        {
            break status;
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .context("killing timed-out Grok worker preflight capability `grok`")?;
            let _ = child.wait();
            // Reap drain threads so a timed-out probe does not leak join handles.
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            bail!(
                "grok models did not complete while waiting for OAuth refresh coordination within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let stdout = stdout_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stdout drain thread panicked for Grok preflight"))?
        .context("reading stdout from Grok worker preflight capability `grok`")?;
    let stderr = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("stderr drain thread panicked for Grok preflight"))?
        .context("reading stderr from Grok worker preflight capability `grok`")?;
    Ok(Output { status, stdout, stderr })
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

    assert_session_store_writable(runner, workspace, environment)?;

    Ok(())
}

/// Absolute path of the probe file, under the sessions directory itself so
/// the write is evaluated exactly where Grok will create its session.
const SESSION_STORE_PROBE_LEAF: &str = "sessions/.boss-preflight-write-probe";

/// Prove Grok can create a file in its own session store *through the exact
/// sandbox the pane will run under* — the one capability whose absence parks
/// the CLI on its start menu instead of failing it outward.
///
/// Grok's session store is a symlink out of `$GROK_HOME` into Boss's state
/// root ([`crate::transcript_store::provision_durable_sessions`]). Seatbelt
/// evaluates the resolved target, so granting `$GROK_HOME` says nothing about
/// whether this write lands: the sandbox can deny it while every path-shaped
/// check in the profile looks right. When it is denied, `grok` starts, reports
/// `Session creation failed: Permission denied.: {"code": "FS_PERMISSION_DENIED",
/// "detail": "Operation not permitted (os error 1)"}`, and then sits at its
/// start menu holding a slot and a cube lease — it never execs a session, so
/// no driver signal is ever emitted and nothing downstream can attribute the
/// stall to a permission fault.
///
/// Running the probe here converts that into a pre-spawn error carrying the
/// kernel's own `Operation not permitted` text, before any pane, slot, or
/// lease is committed. It is a real write rather than a permission
/// calculation, so it stays honest about any future fence — not just the
/// Boss-data-dir one that first caused this.
fn assert_session_store_writable(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
) -> anyhow::Result<()> {
    let probe = environment.grok_home().join(SESSION_STORE_PROBE_LEAF);
    let probe_arg = probe.display().to_string();
    let output = runner.run("/usr/bin/touch", &[&probe_arg], workspace, environment)?;
    // The engine itself is unsandboxed, so it can always clear the probe —
    // and must, so a successful preflight leaves the transcript store clean.
    let _ = std::fs::remove_file(&probe);
    if !output.success {
        bail!(
            "Grok worker preflight failed: Grok session storage at {} is not writable under the worker sandbox; \
             Grok would start and then fail session creation with FS_PERMISSION_DENIED. \
             {}",
            probe.display(),
            rendered_output(&output)
        );
    }
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
            success(""),
        ]);

        run_worker_preflight_with(&runner, workspace, &environment()).unwrap();
        assert_eq!(
            runner.programs.into_inner(),
            ["grok", "cube", "gh", "jj", "jj", "/usr/bin/touch"]
        );
    }

    /// The failure this check exists for: the session store is unwritable
    /// under the sandbox. It must abort the preflight — i.e. abort before a
    /// pane, slot, or lease is committed — and carry the kernel's own text.
    #[test]
    fn an_unwritable_session_store_fails_the_preflight() {
        let runner = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            success(r#"{"workspace":{"workspace_path":"/workspace"}}"#),
            success("Logged in to github.com account worker (keyring)\n"),
            success("/workspace\n"),
            success("origin git@github.com:example/repo.git\n"),
            PreflightOutput {
                success: false,
                status: "exit status: 1".to_owned(),
                stdout: String::new(),
                stderr: "touch: sessions/.boss-preflight-write-probe: Operation not permitted\n".to_owned(),
            },
        ]);

        let error = run_worker_preflight_with(&runner, Path::new("/workspace"), &environment())
            .unwrap_err()
            .to_string();
        assert!(error.contains("not writable under the worker sandbox"), "{error}");
        assert!(error.contains("FS_PERMISSION_DENIED"), "{error}");
        assert!(error.contains("Operation not permitted"), "{error}");
    }

    /// The probe has to target the sessions directory itself — probing
    /// `$GROK_HOME` instead would pass while the symlinked destination stayed
    /// denied, which is precisely the blind spot being closed.
    #[test]
    fn the_probe_writes_through_the_sessions_link() {
        let environment = environment();
        struct CapturingRunner(RefCell<Vec<String>>);
        impl PreflightRunner for CapturingRunner {
            fn run(
                &self,
                _program: &str,
                args: &[&str],
                _workspace: &Path,
                _environment: &GrokProcessEnvironment,
            ) -> anyhow::Result<PreflightOutput> {
                self.0.borrow_mut().extend(args.iter().map(|a| (*a).to_owned()));
                Ok(success(""))
            }
        }

        let runner = CapturingRunner(RefCell::new(Vec::new()));
        assert_session_store_writable(&runner, Path::new("/workspace"), &environment).unwrap();
        let args = runner.0.into_inner();
        assert_eq!(
            args,
            vec![
                environment
                    .grok_home()
                    .join(SESSION_STORE_PROBE_LEAF)
                    .display()
                    .to_string()
            ]
        );
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

    #[test]
    fn run_grok_models_with_timeout_captures_stdout() {
        let mut command = Command::new("/bin/echo");
        command.arg("You are logged in with grok.com.");
        let output = run_grok_models_with_timeout(&mut command, Duration::from_secs(5)).unwrap();
        assert!(output.status.success(), "status={}", output.status);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("You are logged in with grok.com."),
            "stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn run_grok_models_with_timeout_kills_and_reports_deadline() {
        let mut command = Command::new("/bin/sleep");
        command.arg("5");
        let error = run_grok_models_with_timeout(&mut command, Duration::from_millis(200))
            .unwrap_err()
            .to_string();
        assert!(error.contains("within"), "{error}");
        assert!(error.contains("0s") || error.contains("OAuth refresh"), "{error}");
    }

    #[test]
    fn run_grok_models_with_timeout_drains_large_stdout() {
        // ~200KiB exceeds the typical 64KiB pipe buffer; without concurrent drain
        // this would deadlock the try_wait poll loop. Prefer pure shell so the
        // Bazel sandbox need not grant /dev/zero or PATH-resolved helpers.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            // 4000 * 50 = 200_000 bytes of stdout.
            "i=0; while [ \"$i\" -lt 4000 ]; do printf '%050d' \"$i\"; i=$((i + 1)); done",
        ]);
        let output = run_grok_models_with_timeout(&mut command, Duration::from_secs(30)).unwrap();
        assert!(
            output.status.success(),
            "status={} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stdout.len() > 100_000,
            "expected large captured stdout, got {} bytes",
            output.stdout.len()
        );
    }
}
