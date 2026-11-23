//! Fail-fast capability checks for the scoped Grok worker environment.
//!
//! Every subprocess here is bounded: a wall-clock timeout kills the child and
//! its whole process group, and a timeout is a loud preflight failure naming
//! the command — never a silent pass. These calls run synchronously, so the
//! caller ([`super::GrokDriver::provision_workspace`]) must keep them off the
//! async worker threads with `spawn_blocking`; an unbounded call here once
//! pinned two runtime threads for hours and stopped the engine from exiting.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use super::environment::GrokProcessEnvironment;
use super::oauth_refresh::{GROK_LOGIN_AFFIRMATION, with_credential_reprobe};
use crate::ExecutionWorkspaceKind;
use crate::bounded_command::run_bounded_as;

/// Wall-clock limit for the OAuth-sensitive `grok models` probe.
const GROK_MODELS_TIMEOUT: Duration = Duration::from_secs(30);

/// Shortest wait for a live OAuth refresh worth a log line: anything under it
/// is the cost of reading the lock file, not a wait.
const OAUTH_WAIT_REPORT_THRESHOLD: Duration = Duration::from_millis(250);

/// Wall-clock limit for every other preflight subprocess. Generous enough for
/// `cube workspace status` to reach the git remote, short enough that a hung
/// remote fails the dispatch instead of holding a thread for hours.
pub(super) const PREFLIGHT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

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
            let wait_started = Instant::now();
            environment
                .wait_for_auth_ready_for_oauth_probe()
                .context("waiting for a Grok OAuth refresh before preflight")?;
            // This wait is silent when it ends well, and it is counted in the
            // step's time. Report one that actually held the spawn so it is
            // not read as a slow `grok models`.
            let waited = wait_started.elapsed();
            if waited >= OAUTH_WAIT_REPORT_THRESHOLD {
                tracing::info!(
                    grok_home = %environment.grok_home().display(),
                    waited_ms = waited.as_millis() as u64,
                    "Grok spawn preparation waited for a live OAuth refresh before probing the credential"
                );
            }
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

/// One subprocess Grok's spawn preparation ran, and how long it took.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepTiming {
    step: String,
    elapsed: Duration,
}

/// Times every command the wrapped runner executes.
///
/// These subprocesses run on each Grok spawn before its pane exists, and no
/// other driver has an equivalent step, so their cost is logged per command
/// instead of being left inside one opaque provisioning interval.
struct TimingRunner<'a> {
    inner: &'a dyn PreflightRunner,
    steps: RefCell<Vec<StepTiming>>,
}

impl<'a> TimingRunner<'a> {
    fn new(inner: &'a dyn PreflightRunner) -> Self {
        Self {
            inner,
            steps: RefCell::new(Vec::new()),
        }
    }
}

impl PreflightRunner for TimingRunner<'_> {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        workspace: &Path,
        environment: &GrokProcessEnvironment,
    ) -> anyhow::Result<PreflightOutput> {
        let started = Instant::now();
        let result = self.inner.run(program, args, workspace, environment);
        self.steps.borrow_mut().push(StepTiming {
            step: step_label(program, args),
            elapsed: started.elapsed(),
        });
        result
    }
}

/// A short name for a preflight command: the program and its subcommand
/// words, without flags, flag values or paths. `cube --json workspace status
/// --workspace /w` is `cube workspace status`.
fn step_label(program: &str, args: &[&str]) -> String {
    let program = Path::new(program)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_owned());
    let subcommand = args
        .iter()
        .skip_while(|arg| arg.starts_with('-'))
        .take_while(|arg| !arg.starts_with('-') && !arg.starts_with('/'));
    std::iter::once(program.as_str())
        .chain(subcommand.copied())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Log how long one spawn-preparation subprocess took. `grok_home` is the
/// per-run home, whose path carries the execution id.
pub(super) fn report_step(grok_home: &Path, step: &str, elapsed: Duration) {
    tracing::info!(
        grok_home = %grok_home.display(),
        step,
        elapsed_ms = elapsed.as_millis() as u64,
        "Grok spawn preparation step finished"
    );
}

/// Prefix on every preflight error, so a failure reads as what it is wherever
/// it surfaces.
const PREFLIGHT_FAILURE_PREFIX: &str = "Grok worker preflight failed";

/// Run `command` capturing stdout/stderr, killing it and its process group if
/// it has not finished within `timeout`.
///
/// A timeout is an error that names the command; it is never mapped to an
/// empty success.
pub(super) fn run_bounded(command: &mut Command, timeout: Duration) -> anyhow::Result<Output> {
    run_bounded_as(command, timeout, PREFLIGHT_FAILURE_PREFIX)
}

/// Prove that every capability the worker needs is usable before the pane is
/// spawned. Each check requires affirmative output in addition to exit zero;
/// this matters because `grok models` returns zero while printing "not
/// authenticated" when no credential is usable.
pub fn run_worker_preflight(
    workspace: &Path,
    environment: &GrokProcessEnvironment,
    kind: ExecutionWorkspaceKind,
) -> anyhow::Result<()> {
    let runner = TimingRunner::new(&RealPreflightRunner);
    let result = run_worker_preflight_with(&runner, workspace, environment, kind);
    // Reported for a failed preflight too: the steps before the failure are
    // where its time went.
    for timing in runner.steps.into_inner() {
        report_step(environment.grok_home(), &timing.step, timing.elapsed);
    }
    result
}

fn run_worker_preflight_with(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
    kind: ExecutionWorkspaceKind,
) -> anyhow::Result<()> {
    assert_grok_oauth_with_reprobe(runner, workspace, environment)?;

    match kind {
        ExecutionWorkspaceKind::Cube => probe_cube_workspace(runner, workspace, environment)?,
        ExecutionWorkspaceKind::Scratch => {
            // Enter and enumerate the actual directory with the worker's tool
            // environment. Scratch runs have no Cube registry entry or checkout.
            let output = runner.run("/bin/ls", &["-a", "."], workspace, environment)?;
            require_success("Scratch workspace access", &output)?;
            if !output.stdout.lines().any(|line| line == ".") {
                bail!("Grok worker preflight failed: scratch directory listing omitted .");
            }
        }
    }

    let gh = runner.run(
        "gh",
        &["auth", "status", "--active", "--hostname", "github.com"],
        workspace,
        environment,
    )?;
    assert_gh_keyring(&gh)?;

    if kind == ExecutionWorkspaceKind::Cube {
        let jj_root = runner.run("jj", &["root"], workspace, environment)?;
        assert_jj_root(&jj_root, workspace)?;

        let jj_remotes = runner.run("jj", &["git", "remote", "list"], workspace, environment)?;
        assert_jj_remotes(&jj_remotes)?;
    }

    assert_session_store_writable(runner, workspace, environment)?;

    Ok(())
}

/// Prove the Boss-bundled `cube` can see the leased workspace.
///
/// `cube` is run by the absolute path of the binary that ships with the engine
/// ([`GrokProcessEnvironment::bundled_cube`]), never by bare name: the engine's
/// own `PATH` can put the user's repobin shim ahead of the bundle, and that
/// shim builds the CLI from source with Bazel. When the bundled binary is
/// missing, or genuinely cannot do the job, this still fails loudly.
fn probe_cube_workspace(
    runner: &dyn PreflightRunner,
    workspace: &Path,
    environment: &GrokProcessEnvironment,
) -> anyhow::Result<()> {
    let cube_program = environment.bundled_cube()?.to_string_lossy().into_owned();
    let workspace_arg = workspace.display().to_string();
    let cube = runner.run(
        &cube_program,
        &["--json", "workspace", "status", "--workspace", &workspace_arg],
        workspace,
        environment,
    )?;
    assert_cube_workspace(&cube, workspace)
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
    let output = with_credential_reprobe(
        environment.auth_path(),
        || runner.run("grok", &["models"], workspace, environment),
        |output| assert_grok_oauth(output).is_ok(),
    )?;
    assert_grok_oauth(&output)
}

fn assert_grok_oauth(output: &PreflightOutput) -> anyhow::Result<()> {
    require_success("Grok OAuth", output)?;
    if !output.stdout.contains(GROK_LOGIN_AFFIRMATION) {
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

        run_worker_preflight_with(&runner, workspace, &environment(), ExecutionWorkspaceKind::Cube).unwrap();
        assert_eq!(
            runner.programs.into_inner(),
            ["grok", "/bundle/bin/cube", "gh", "jj", "jj", "/usr/bin/touch"]
        );
    }

    #[test]
    fn progression_scratch_preflight_needs_no_cube_or_checkout() {
        let runner = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            success(".\n..\n.grok\n"),
            success("Logged in to github.com account worker (keyring)\n"),
            success(""),
        ]);
        // No bundle is required when dispatch explicitly supplies a scratch workspace.
        let environment = environment().with_bundled_cube(None);
        run_worker_preflight_with(
            &runner,
            Path::new("/arbitrary-directory"),
            &environment,
            ExecutionWorkspaceKind::Scratch,
        )
        .unwrap();
        assert_eq!(
            runner.programs.into_inner(),
            ["grok", "/bin/ls", "gh", "/usr/bin/touch"]
        );
        assert!(runner.outputs.into_inner().is_empty());
    }

    #[test]
    fn cube_workspace_registry_failure_is_not_treated_as_scratch() {
        let runner = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            PreflightOutput {
                success: false,
                status: "exit status: 5".into(),
                stdout: String::new(),
                stderr: "workspace is not tracked".into(),
            },
        ]);
        let error = run_worker_preflight_with(
            &runner,
            Path::new("/scratch-looking-directory"),
            &environment(),
            ExecutionWorkspaceKind::Cube,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Cube workspace access command exited exit status: 5"),
            "{error}"
        );
        assert_eq!(runner.programs.into_inner(), ["grok", "/bundle/bin/cube"]);
    }

    #[test]
    fn inaccessible_scratch_directory_fails_preflight() {
        let runner = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            PreflightOutput {
                success: false,
                status: "exit status: 1".into(),
                stdout: String::new(),
                stderr: "Permission denied".into(),
            },
        ]);
        let error = run_worker_preflight_with(
            &runner,
            Path::new("/scratch"),
            &environment(),
            ExecutionWorkspaceKind::Scratch,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Scratch workspace access command exited"), "{error}");
    }

    #[test]
    fn step_labels_name_the_command_without_flags_or_paths() {
        assert_eq!(step_label("grok", &["models"]), "grok models");
        assert_eq!(
            step_label(
                "/bundle/bin/cube",
                &["--json", "workspace", "status", "--workspace", "/workspace"]
            ),
            "cube workspace status"
        );
        assert_eq!(
            step_label("gh", &["auth", "status", "--active", "--hostname", "github.com"]),
            "gh auth status"
        );
        assert_eq!(step_label("jj", &["git", "remote", "list"]), "jj git remote list");
        assert_eq!(step_label("/usr/bin/touch", &["/home/sessions/.probe"]), "touch");
        assert_eq!(step_label("/bin/ls", &["-a", "."]), "ls .");
    }

    /// Sleeps a different length for each command, so a time recorded against
    /// the wrong step falls short of that step's own floor.
    struct PacedRunner {
        outputs: RefCell<VecDeque<(Duration, PreflightOutput)>>,
    }

    impl PreflightRunner for PacedRunner {
        fn run(
            &self,
            program: &str,
            _args: &[&str],
            _workspace: &Path,
            _environment: &GrokProcessEnvironment,
        ) -> anyhow::Result<PreflightOutput> {
            let (pause, output) = self
                .outputs
                .borrow_mut()
                .pop_front()
                .with_context(|| format!("no paced output for {program}"))?;
            std::thread::sleep(pause);
            Ok(output)
        }
    }

    #[test]
    fn every_preflight_command_is_timed_in_the_order_it_ran() {
        let ms = Duration::from_millis;
        let paced = PacedRunner {
            outputs: RefCell::new(
                vec![
                    (ms(60), success("You are logged in with grok.com.\n")),
                    (ms(50), success(r#"{"workspace":{"workspace_path":"/workspace"}}"#)),
                    (ms(40), success("Logged in to github.com account worker (keyring)\n")),
                    (ms(30), success("/workspace\n")),
                    (ms(20), success("origin git@github.com:example/repo.git\n")),
                    (ms(10), success("")),
                ]
                .into(),
            ),
        };
        let runner = TimingRunner::new(&paced);

        run_worker_preflight_with(
            &runner,
            Path::new("/workspace"),
            &environment(),
            ExecutionWorkspaceKind::Cube,
        )
        .unwrap();

        let steps = runner.steps.into_inner();
        assert_eq!(
            steps.iter().map(|timing| timing.step.as_str()).collect::<Vec<_>>(),
            [
                "grok models",
                "cube workspace status",
                "gh auth status",
                "jj root",
                "jj git remote list",
                "touch"
            ]
        );
        // Floors only: a sleep never returns early, and nothing bounds how
        // late a loaded host lets it return.
        for (timing, floor) in steps.iter().zip([60, 50, 40, 30, 20, 10]) {
            assert!(timing.elapsed >= ms(floor), "{timing:?} is under {floor} ms");
        }
    }

    /// A check that fails still has its time recorded, along with the checks
    /// before it: that is where a slow failure spent its time.
    #[test]
    fn a_failing_command_is_still_timed() {
        let failing = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            PreflightOutput {
                success: false,
                status: "exit status: 5".to_owned(),
                stdout: String::new(),
                stderr: "workspace is not tracked".to_owned(),
            },
        ]);
        let runner = TimingRunner::new(&failing);

        run_worker_preflight_with(
            &runner,
            Path::new("/workspace"),
            &environment(),
            ExecutionWorkspaceKind::Cube,
        )
        .unwrap_err();

        let steps = runner.steps.into_inner();
        assert_eq!(
            steps.iter().map(|timing| timing.step.as_str()).collect::<Vec<_>>(),
            ["grok models", "cube workspace status"]
        );
    }

    /// A scratch workspace runs a different, shorter set of commands, and the
    /// timings follow whatever ran.
    #[test]
    fn a_scratch_preflight_times_the_commands_it_runs() {
        let scratch = FakeRunner::new(vec![
            success("You are logged in with grok.com.\n"),
            success(".\n..\n.grok\n"),
            success("Logged in to github.com account worker (keyring)\n"),
            success(""),
        ]);
        let runner = TimingRunner::new(&scratch);

        run_worker_preflight_with(
            &runner,
            Path::new("/scratch"),
            &environment().with_bundled_cube(None),
            ExecutionWorkspaceKind::Scratch,
        )
        .unwrap();

        let steps = runner.steps.into_inner();
        assert_eq!(
            steps.iter().map(|timing| timing.step.as_str()).collect::<Vec<_>>(),
            ["grok models", "ls .", "gh auth status", "touch"]
        );
    }

    /// With no bundled `cube`, the preflight must fail loudly and name the
    /// problem — before it runs anything, and without substituting a `PATH`
    /// lookup of the bare name.
    #[test]
    fn a_missing_bundled_cube_fails_the_preflight_without_a_path_fallback() {
        let runner = FakeRunner::new(vec![success("You are logged in with grok.com.\n")]);
        let environment = environment().with_bundled_cube(None);

        let error = run_worker_preflight_with(
            &runner,
            Path::new("/workspace"),
            &environment,
            ExecutionWorkspaceKind::Cube,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("Boss-bundled `cube` could not be located"), "{error}");
        assert!(error.contains("refusing a PATH lookup"), "{error}");
        assert_eq!(
            runner.programs.into_inner(),
            ["grok"],
            "nothing past the OAuth probe may run, and no bare `cube` may be tried"
        );
    }

    /// Runs real processes the way the engine does, but with a `PATH` whose
    /// first entry holds a decoy `cube` — the shape of `~/bin/cube`, the repobin
    /// shim that wins a bare-name lookup on a developer machine.
    struct DecoyPathRunner {
        decoy_dir: PathBuf,
    }

    impl PreflightRunner for DecoyPathRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            workspace: &Path,
            _environment: &GrokProcessEnvironment,
        ) -> anyhow::Result<PreflightOutput> {
            // Another test thread can fork while a freshly written script is still open
            // for writing, which makes exec fail with ETXTBSY until the forked child execs.
            let output = boss_command_runner::retry_on_etxtbsy(|| {
                Command::new(program)
                    .args(args)
                    .current_dir(workspace)
                    .env("PATH", format!("{}:/usr/bin:/bin", self.decoy_dir.display()))
                    .output()
            })?;
            Ok(PreflightOutput {
                success: output.status.success(),
                status: output.status.to_string(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    #[cfg(unix)]
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Regression: the cube probe ran a bare `cube`, so a repobin shim earlier on
    /// the engine's `PATH` tried to build `//tools/cube:cube` from source and
    /// killed the run before the pane started. The decoy here records that it ran
    /// and fails; the bundled copy answers properly.
    #[cfg(unix)]
    #[test]
    fn the_cube_probe_runs_the_bundled_binary_not_a_decoy_earlier_on_path() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let decoy_dir = root.path().join("home-bin");
        let marker = root.path().join("decoy-ran");
        write_script(
            &decoy_dir.join("cube"),
            &format!(
                "#!/bin/sh\ntouch '{}'\necho 'repobin: building //tools/cube:cube' >&2\nexit 1\n",
                marker.display()
            ),
        );
        let bundled = root.path().join("Boss.app/Contents/Resources/bin/cube");
        write_script(
            &bundled,
            &format!(
                "#!/bin/sh\nprintf '%s' '{{\"workspace\":{{\"workspace_path\":\"{}\"}}}}'\n",
                workspace.display()
            ),
        );

        let environment = environment().with_bundled_cube(Some(bundled));
        let runner = DecoyPathRunner { decoy_dir };
        probe_cube_workspace(&runner, &workspace, &environment).unwrap();
        assert!(!marker.exists(), "the decoy `cube` on PATH must never be executed");
    }

    /// The probe must keep failing when the bundled `cube` itself cannot do its
    /// job — a decoy must not mask that either.
    #[cfg(unix)]
    #[test]
    fn a_failing_bundled_cube_still_fails_the_probe_loudly() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let decoy_dir = root.path().join("home-bin");
        write_script(
            &decoy_dir.join("cube"),
            &format!(
                "#!/bin/sh\nprintf '%s' '{{\"workspace\":{{\"workspace_path\":\"{}\"}}}}'\n",
                workspace.display()
            ),
        );
        let bundled = root.path().join("bundle/cube");
        write_script(&bundled, "#!/bin/sh\necho 'bundled cube is broken' >&2\nexit 7\n");

        let environment = environment().with_bundled_cube(Some(bundled));
        let error = probe_cube_workspace(&DecoyPathRunner { decoy_dir }, &workspace, &environment)
            .unwrap_err()
            .to_string();
        assert!(error.contains("Cube workspace access command exited"), "{error}");
        assert!(error.contains("bundled cube is broken"), "{error}");
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

        let error = run_worker_preflight_with(
            &runner,
            Path::new("/workspace"),
            &environment(),
            ExecutionWorkspaceKind::Cube,
        )
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
        let error = run_bounded(&mut command, Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "timeout did not bound the call"
        );
        assert!(error.contains("Grok worker preflight failed"), "{error}");
        assert!(error.contains("`/bin/sleep 60`"), "{error}");
        assert!(error.contains("did not complete within"), "{error}");
    }
}
