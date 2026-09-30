//! Fail-fast capability checks for the scoped Grok worker environment.
//!
//! Every subprocess here is bounded: a wall-clock timeout kills the child and
//! its whole process group, and a timeout is a loud preflight failure naming
//! the command — never a silent pass. These calls run synchronously, so the
//! caller ([`super::GrokDriver::provision_workspace`]) must keep them off the
//! async worker threads with `spawn_blocking`; an unbounded call here once
//! pinned two runtime threads for hours and stopped the engine from exiting.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};

use super::environment::GrokProcessEnvironment;

/// Wall-clock limit for the OAuth-sensitive `grok models` probe.
const GROK_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

/// Wall-clock limit for every other preflight subprocess. Generous enough for
/// `cube workspace status` to reach the git remote, short enough that a hung
/// remote fails the dispatch instead of holding a thread for hours.
pub(super) const PREFLIGHT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Preflight subprocesses currently running, keyed by pid (which is also the
/// process-group id), so an engine shutdown can reap and report them.
#[derive(Default)]
struct InFlight(Mutex<BTreeMap<u32, String>>);

impl InFlight {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<u32, String>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Kill every registered process group and return the command lines.
    fn abandon(&self) -> Vec<String> {
        std::mem::take(&mut *self.map())
            .into_iter()
            .map(|(pgid, command)| {
                boss_command_runner::kill_process_group(pgid);
                command
            })
            .collect()
    }
}

static IN_FLIGHT: InFlight = InFlight(Mutex::new(BTreeMap::new()));

/// Kill every preflight subprocess still running and return their command
/// lines. Called when the engine gives up waiting for blocking tasks at
/// shutdown, so abandoned probes do not outlive the engine as orphans.
pub fn abandon_in_flight_preflight_commands() -> Vec<String> {
    IN_FLIGHT.abandon()
}

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

struct RealPreflightRunner;

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
        let mut command = Command::new(program);
        command.args(args).current_dir(workspace);
        let timeout = if program == "grok" {
            environment.apply_to_command(&mut command);
            GROK_MODELS_TIMEOUT
        } else {
            environment.apply_tool_sandbox_environment(&mut command);
            PREFLIGHT_COMMAND_TIMEOUT
        };
        let output = run_bounded(&mut command, timeout)?;
        Ok(PreflightOutput {
            success: output.status.success(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Human-readable `program arg arg` for errors and the in-flight registry.
fn render_command(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Run `command` capturing stdout/stderr, killing it and its process group if
/// it has not finished within `timeout`.
///
/// A timeout is an error that names the command; it is never mapped to an
/// empty success. Output is drained concurrently with the wait (see
/// [`boss_command_runner::output_blocking_timeout_in_group`]) so a chatty child
/// cannot deadlock against a non-reading parent.
pub(super) fn run_bounded(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    run_bounded_in(&IN_FLIGHT, command, timeout)
}

fn run_bounded_in(in_flight: &InFlight, command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    let rendered = render_command(command);
    let registered = std::cell::Cell::new(None);
    let result = boss_command_runner::output_blocking_timeout_in_group(command, timeout, |pid| {
        in_flight.map().insert(pid, rendered.clone());
        registered.set(Some(pid));
    });
    if let Some(pid) = registered.get() {
        in_flight.map().remove(&pid);
    }
    result.map_err(|err| {
        if err.kind() == std::io::ErrorKind::TimedOut {
            anyhow::anyhow!(
                "Grok worker preflight failed: `{rendered}` did not complete within {}s and was killed \
                 along with its process group",
                timeout.as_secs()
            )
        } else {
            anyhow::Error::new(err).context(format!("running Grok worker preflight capability `{rendered}`"))
        }
    })
}

/// Prove that every capability the worker needs is usable before the pane is
/// spawned. Each check requires affirmative output in addition to exit zero;
/// this matters because `grok models` returns zero while printing "not
/// authenticated" when no credential is usable.
pub fn run_worker_preflight(workspace: &Path, environment: &GrokProcessEnvironment) -> anyhow::Result<()> {
    run_worker_preflight_with(&RealPreflightRunner, workspace, environment)
}

fn run_worker_preflight_with(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
) -> anyhow::Result<()> {
    assert_grok_oauth_with_reprobe(runner, workspace, environment)?;

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

/// Prove Grok can create a file in its own session store before the pane is
/// spawned — the one capability whose absence parks the CLI on its start menu
/// instead of failing it outward.
///
/// Grok's session store is a symlink out of `$GROK_HOME` into Boss's state
/// root ([`crate::transcript_store::provision_durable_sessions`]). When it is
/// unavailable, `grok` starts, reports
/// `Session creation failed: Permission denied.: {"code": "FS_PERMISSION_DENIED",
/// "detail": "Operation not permitted (os error 1)"}`, and then sits at its
/// start menu holding a slot and a cube lease — it never execs a session, so
/// no driver signal is ever emitted and nothing downstream can attribute the
/// stall to a permission fault.
///
/// Running the probe here converts that into a pre-spawn error carrying the
/// kernel's own `Operation not permitted` text, before any pane, slot, or
/// lease is committed. It is a real write rather than a permission
/// calculation, so it stays honest about the actual process environment.
fn assert_session_store_writable(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
) -> anyhow::Result<()> {
    let probe = environment.grok_home().join(SESSION_STORE_PROBE_LEAF);
    let probe_arg = probe.display().to_string();
    let output = runner.run("/usr/bin/touch", &[&probe_arg], workspace, environment)?;
    // The engine clears the probe so a successful preflight leaves the
    // transcript store clean.
    let _ = std::fs::remove_file(&probe);
    if !output.success {
        bail!(
            "Grok worker preflight failed: Grok session storage at {} is not writable by the worker; \
             Grok would start and then fail session creation with FS_PERMISSION_DENIED. \
             {}",
            probe.display(),
            rendered_output(&output)
        );
    }
    Ok(())
}

/// Cheap, content-blind snapshot of the shared OAuth credential's on-disk
/// state, used to detect whether a refresh rewrote the file during the
/// probe. Never reads the credential bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthFingerprint {
    modified: SystemTime,
    len: u64,
}

fn fingerprint_auth_credential(auth_path: &Path) -> Option<AuthFingerprint> {
    let metadata = std::fs::metadata(auth_path).ok()?;
    let modified = metadata.modified().ok()?;
    Some(AuthFingerprint {
        modified,
        len: metadata.len(),
    })
}

/// Run the Grok OAuth probe and assert its affirmation, tolerating exactly
/// one race: the probe's own invocation of `grok models` can refresh an
/// expired-but-refreshable shared token as a side effect, so that first run
/// renders its authentication banner from the pre-refresh state even though
/// the credential is valid by the time the process exits. Detected by
/// fingerprinting the credential file before and after the probe — never by
/// assuming a failure implies a refresh — and bounded to a single re-probe:
/// no observed rewrite means no retry, so a genuine logout or an expired
/// refresh token still fails on the first assertion, exactly as before.
fn assert_grok_oauth_with_reprobe(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
) -> anyhow::Result<()> {
    let before = fingerprint_auth_credential(environment.auth_path());
    let output = runner.run("grok", &["models"], workspace, environment)?;
    if assert_grok_oauth(&output).is_ok() {
        return Ok(());
    }
    let after = fingerprint_auth_credential(environment.auth_path());
    if before.is_none() || before == after {
        return assert_grok_oauth(&output);
    }
    tracing::info!(
        auth_path = %environment.auth_path().display(),
        "Grok OAuth credential was rewritten during the preflight probe (likely an expiry refresh); re-probing once"
    );
    let output = runner.run("grok", &["models"], workspace, environment)?;
    assert_grok_oauth(&output)
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
        assert!(error.contains("not writable by the worker"), "{error}");
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
        let output = success("You are not authenticated.\nDefault model: grok-4.6\n");
        let error = assert_grok_oauth(&output).unwrap_err().to_string();
        assert!(error.contains("Grok OAuth is unavailable"), "{error}");
    }

    /// Simulates `grok models`: each queued response is optionally paired
    /// with a rewrite of the on-disk credential, standing in for the CLI's
    /// own OAuth refresh side effect during the probe.
    struct GrokReprobeRunner {
        auth_path: PathBuf,
        responses: RefCell<VecDeque<(PreflightOutput, bool)>>,
        calls: RefCell<u32>,
    }

    impl GrokReprobeRunner {
        fn new(auth_path: PathBuf, responses: Vec<(PreflightOutput, bool)>) -> Self {
            Self {
                auth_path,
                responses: RefCell::new(responses.into()),
                calls: RefCell::new(0),
            }
        }
    }

    impl PreflightRunner for GrokReprobeRunner {
        fn run(
            &self,
            program: &str,
            _args: &[&str],
            _workspace: &Path,
            _environment: &GrokProcessEnvironment,
        ) -> anyhow::Result<PreflightOutput> {
            assert_eq!(program, "grok");
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            let (output, rewrite) = self
                .responses
                .borrow_mut()
                .pop_front()
                .expect("no fake grok response queued");
            if rewrite {
                // Length grows with the call count so the fingerprint's size
                // check can't be flaked by same-second mtime resolution.
                std::fs::write(&self.auth_path, "x".repeat(*calls as usize * 16)).unwrap();
            }
            Ok(output)
        }
    }

    fn environment_with_auth_path(auth_path: PathBuf) -> GrokProcessEnvironment {
        GrokProcessEnvironment::for_test_with_auth_path(auth_path)
    }

    /// The scenario this fix exists for: the probe's own side effect
    /// refreshes an expired-but-refreshable token underneath it, so the
    /// first run reports pre-refresh state. A rewrite observed via the
    /// on-disk fingerprint earns exactly one re-probe, and that re-probe's
    /// affirmation is what the preflight trusts.
    #[test]
    fn reprobe_succeeds_after_evidenced_refresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth_path = tmp.path().join("auth.json");
        std::fs::write(&auth_path, "seed").unwrap();
        let runner = GrokReprobeRunner::new(
            auth_path.clone(),
            vec![
                (success("You are not authenticated.\nDefault model: grok-4.6\n"), true),
                (success("You are logged in with grok.com.\n"), false),
            ],
        );

        let environment = environment_with_auth_path(auth_path);
        assert_grok_oauth_with_reprobe(&runner, Path::new("/workspace"), &environment).unwrap();
        assert_eq!(*runner.calls.borrow(), 2);
    }

    /// A genuine logout, revoked credential, or expired refresh token
    /// rewrites nothing. No evidenced refresh means no retry: this must fail
    /// exactly as it did before the fix, on the first probe alone.
    #[test]
    fn no_reprobe_when_credential_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth_path = tmp.path().join("auth.json");
        std::fs::write(&auth_path, "seed").unwrap();
        let runner = GrokReprobeRunner::new(
            auth_path.clone(),
            vec![(success("You are not authenticated.\nDefault model: grok-4.6\n"), false)],
        );

        let environment = environment_with_auth_path(auth_path);
        let error = assert_grok_oauth_with_reprobe(&runner, Path::new("/workspace"), &environment)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Grok OAuth is unavailable"), "{error}");
        assert_eq!(
            *runner.calls.borrow(),
            1,
            "must not re-probe without an evidenced rewrite"
        );
    }

    /// The common case: a healthy credential affirms on the first probe, so
    /// no fingerprint comparison or re-probe is needed at all.
    #[test]
    fn healthy_credential_probes_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let auth_path = tmp.path().join("auth.json");
        std::fs::write(&auth_path, "seed").unwrap();
        let runner = GrokReprobeRunner::new(
            auth_path.clone(),
            vec![(success("You are logged in with grok.com.\n"), false)],
        );

        let environment = environment_with_auth_path(auth_path);
        assert_grok_oauth_with_reprobe(&runner, Path::new("/workspace"), &environment).unwrap();
        assert_eq!(*runner.calls.borrow(), 1);
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
    fn run_bounded_captures_stdout() {
        let mut command = Command::new("/bin/echo");
        command.arg("You are logged in with grok.com.");
        let output = run_bounded(&mut command, Duration::from_secs(5)).unwrap();
        assert!(output.status.success(), "status={}", output.status);
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("You are logged in with grok.com."),
            "stdout={:?}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    /// A hung preflight command is killed at the timeout and surfaces as a
    /// preflight failure that names the command — it is not a silent pass.
    #[test]
    fn a_hung_command_is_killed_and_fails_the_preflight_naming_it() {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let started = std::time::Instant::now();
        let registry = InFlight::default();
        let error = run_bounded_in(&registry, &mut command, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timeout did not bound the call"
        );
        assert!(error.contains("Grok worker preflight failed"), "{error}");
        assert!(error.contains("`/bin/sleep 60`"), "{error}");
        assert!(error.contains("did not complete within"), "{error}");
        assert!(registry.map().is_empty(), "timed-out command must leave the registry");
    }

    /// The real hang: a child wedged on a grandchild (`cube` → `git ls-remote`)
    /// must not leave that grandchild behind.
    #[test]
    fn a_timeout_kills_the_grandchild_too() {
        let pid_file = std::env::temp_dir().join(format!("boss-preflight-grandchild-{}", std::process::id()));
        let _ = std::fs::remove_file(&pid_file);
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pid_file.display()));
        run_bounded(&mut command, Duration::from_millis(500)).unwrap_err();
        let grandchild: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        let _ = std::fs::remove_file(&pid_file);
        let alive = || unsafe { libc::kill(grandchild, 0) == 0 };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "grandchild {grandchild} survived the preflight timeout");
    }

    #[test]
    fn abandoning_in_flight_commands_kills_and_reports_them() {
        let mut command = Command::new("/bin/sleep");
        command.arg("61");
        let registry = std::sync::Arc::new(InFlight::default());
        let runner = {
            let registry = registry.clone();
            std::thread::spawn(move || run_bounded_in(&registry, &mut command, Duration::from_secs(120)))
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while registry.map().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let abandoned = registry.abandon();
        assert!(abandoned.iter().any(|c| c == "/bin/sleep 61"), "{abandoned:?}");
        // The killed child exits with a signal status rather than hanging.
        let output = runner.join().unwrap().unwrap();
        assert!(!output.status.success());
    }

    #[test]
    fn run_bounded_drains_large_stdout() {
        // ~200KiB exceeds the typical 64KiB pipe buffer; without concurrent drain
        // this would deadlock the wait. Prefer pure shell so the Bazel sandbox
        // need not grant /dev/zero or PATH-resolved helpers.
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            // 4000 * 50 = 200_000 bytes of stdout.
            "i=0; while [ \"$i\" -lt 4000 ]; do printf '%050d' \"$i\"; i=$((i + 1)); done",
        ]);
        let output = run_bounded(&mut command, Duration::from_secs(30)).unwrap();
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
