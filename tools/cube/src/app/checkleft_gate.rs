//! The pre-push `checkleft` gate: locate the binary, run it, and refuse the
//! push when it reports findings.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::errors::{CubeError, Result};
use crate::app::stage::run_stage;
use crate::command_runner::{CommandInvocation, RealCommandRunner};

/// Upper bound on the `repobin exec checkleft --version` probe, which pays a
/// cold dispatch-cache miss (a full `bazel build //tools/checkleft:checkleft`)
/// on the first call after a source change. Bazel can otherwise block
/// indefinitely on a held output-base lock, a wedged server, or a stalled
/// repository-rule fetch -- a realistic failure mode on a host running
/// several cube workspaces concurrently. A timeout here is treated the same
/// as a failed probe: resolution falls through to the legacy
/// `<cwd>/bin/checkleft` / PATH lookup.
const CHECKLEFT_PROBE_TIMEOUT: Duration = Duration::from_secs(240);

/// Upper bound on the real `checkleft run` invocation. By the time this
/// runs, the probe above has already warmed the dispatch cache, so this is
/// bounded generously above the ~85-95s measured runtime while still
/// guaranteeing the gate cannot hang a push forever on a wedged bazel.
const CHECKLEFT_RUN_TIMEOUT: Duration = Duration::from_secs(300);

/// Run the repository's checkleft against the outgoing changes before a
/// push, refusing the push when checkleft reports errors.
///
/// This is the ergonomic, sanctioned-flow half of the "run checkleft
/// before every PR push" guard — the same enforcement the Boss runtime
/// applies to raw `jj git push` is applied here for `cube pr create` /
/// `cube pr update`, and it is the *only* gate that runs for a rung-0
/// conflict-ladder push (`CubeClient::push_resolution` calls straight into
/// `cube --json workspace push`, with no Claude Code session and therefore
/// no `PreToolUse` hook involved). Single source of truth: it shells out to
/// checkleft and trusts its exit code (0 = clean, non-zero = errors) — no
/// policy logic is duplicated. checkleft's own "no CHECKS.yaml → exit 0"
/// behaviour means repos without convention checks pass transparently.
///
/// Bypass is checkleft's own `BYPASS_<CHECK>=<reason>` directives in the
/// commit message / PR description; there is no separate cube-level
/// override.
///
/// Fail-open by construction: when no checkleft binary is found the gate
/// is a no-op, but a clear warning is emitted to stderr so the skip is
/// visible rather than silent. The only refusal is a checkleft that
/// actually reported errors. Resolution order: see [`resolve_checkleft_bin`].
///
/// `pr_description` is the PR body text this push is about to submit, when
/// known (e.g. `cube pr create --body`/`--body-file`). It is exported to the
/// checkleft subprocess as `CHECKS_PR_DESCRIPTION`, which checkleft honours
/// at its highest precedence (see `resolve_pr_description` in
/// `tools/checkleft/src/main.rs`) — without it, `changeset`-scope checks
/// that read the PR description have nothing to scan at `pr create` time,
/// because the PR does not exist yet and no CI env vars are set locally.
/// Pass `None` when there is no PR body to check (e.g. `cube pr update` /
/// `cube pr push`, where the PR already exists and checkleft's own
/// branch→PR lookup resolves the real description).
pub(super) fn run_checkleft_gate(cwd: &Path, pr_description: Option<&str>) -> Result<()> {
    run_stage("running checkleft push-gate", || {
        run_checkleft_gate_impl(cwd, resolve_checkleft_bin(cwd), pr_description)
    })
}

/// Inner implementation: runs `checkleft run` using the given argv prefix
/// (program plus any leading args, e.g. `[repobin, "exec", "checkleft"]`),
/// or emits a skip warning and returns `Ok(())` when `checkleft` is `None`.
/// Separated from [`run_checkleft_gate`] so tests can inject a pre-resolved
/// command without modifying global PATH.
pub(super) fn run_checkleft_gate_impl(
    cwd: &Path,
    checkleft: Option<Vec<PathBuf>>,
    pr_description: Option<&str>,
) -> Result<()> {
    let Some(checkleft) = checkleft else {
        eprintln!(
            "cube: checkleft not found via CUBE_CHECKLEFT_BIN, repobin, {}/bin/checkleft, or PATH \
             — push gate SKIPPED",
            cwd.display()
        );
        return Ok(());
    };
    let Some((program, leading_args)) = checkleft.split_first() else {
        eprintln!("cube: checkleft resolution produced an empty command — push gate SKIPPED");
        return Ok(());
    };

    let mut args: Vec<String> = leading_args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    args.push("run".to_string());
    // Explicitly clear CHECKS_PR_DESCRIPTION rather than merely not-setting it
    // when `pr_description` is `None`: without this, the subprocess would
    // inherit whatever CHECKS_PR_DESCRIPTION happens to be set in cube's own
    // environment (e.g. left over from a wrapper or an earlier step), silently
    // overriding checkleft's own branch->PR lookup instead of falling through
    // to it as documented.
    let env = pr_description
        .map(|pr_description| vec![("CHECKS_PR_DESCRIPTION".to_string(), pr_description.to_string())])
        .unwrap_or_default();
    let env_remove: &[&str] = if pr_description.is_some() {
        &[]
    } else {
        &["CHECKS_PR_DESCRIPTION"]
    };
    let invocation = CommandInvocation {
        cwd: cwd.to_path_buf(),
        program: program.to_string_lossy().into_owned(),
        args,
        env,
    };
    let output =
        match RealCommandRunner.run_with_timeout_capturing_output(&invocation, env_remove, CHECKLEFT_RUN_TIMEOUT) {
            Ok(Some(output)) => output,
            // Ran past its budget (a wedged bazel build) — fail open rather than
            // block a push on an infrastructure problem unrelated to the change.
            Ok(None) => {
                eprintln!(
                    "cube: checkleft run did not finish within {}s — push gate SKIPPED",
                    CHECKLEFT_RUN_TIMEOUT.as_secs()
                );
                return Ok(());
            }
            // Could not execute checkleft at all (missing/non-executable binary,
            // permission denied) — the process never started, so report that
            // distinctly rather than implying it ran and timed out.
            Err(e) => {
                eprintln!(
                    "cube: could not execute checkleft ({}): {e} — push gate SKIPPED",
                    program.display()
                );
                return Ok(());
            }
        };
    if output.status.success() {
        return Ok(());
    }

    // checkleft prints its findings to stdout; the CommandFailed path of
    // the shared runner only keeps stderr, so we run checkleft directly to
    // surface the findings in the refusal.
    let findings = String::from_utf8_lossy(&output.stdout);
    let findings = findings.trim();
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    // Empty stdout with non-empty stderr means checkleft exited nonzero before
    // producing any findings — this is an internal/operational error (e.g. a
    // VCS detection failure), not a policy violation. Use a clearly distinct
    // message so users don't try to fix policy or reach for BYPASS unnecessarily.
    if findings.is_empty() {
        return Err(CubeError::InvalidArgument(format!(
            "Push blocked: checkleft internal error — this is a bug, not a policy \
             violation. Please report it.\n\n{stderr}"
        )));
    }
    Err(CubeError::InvalidArgument(format!(
        "checkleft found errors that must be fixed before pushing to GitHub:\n\n{findings}\n\n\
         Fix the findings above and retry. If a finding is a genuine false positive, add a \
         `BYPASS_<CHECK_NAME>=<reason>` line to your commit message or the PR description \
         (the PR description wins), then retry."
    )))
}

/// Resolve the checkleft binary to run for the push gate. Returns the argv
/// prefix (program plus any leading args) that runs checkleft's `run`
/// subcommand, or `None` when no checkleft is available (the gate then
/// no-ops with a warning).
///
/// Resolution order matches the Claude Code `PreToolUse` push-guard script
/// (`resolve_checkleft_command` in `tools/boss/engine/core/src/worker_setup.rs`)
/// — the other checkleft push gate in the repo, which runs for interactive
/// worker pushes rather than the rung-0/`cube pr` pushes this gate covers:
///   1. `CUBE_CHECKLEFT_BIN` env override (explicit path, used as-is)
///   2. `repobin exec checkleft` via a `repobin` found at `<cwd>/bin/repobin`
///      or on `PATH`, gated on a `repobin exec checkleft --version` probe
///      succeeding first (see [`probe_repobin_checkleft`]) — a bare
///      `checkleft` on `PATH` can silently resolve to an unrelated, stale
///      build (e.g. an old `cargo install checkleft`) that still runs and
///      still exits non-zero with `unknown implementation` errors for every
///      check added since that build, so repobin's build-from-source
///      dispatch is preferred when it is actually working
///   3. `<cwd>/bin/checkleft` (repobin-installed artifact)
///   4. `checkleft` on PATH (installed globally or via PATH-based repobin)
pub(super) fn resolve_checkleft_bin(cwd: &Path) -> Option<Vec<PathBuf>> {
    if let Some(override_path) = std::env::var_os("CUBE_CHECKLEFT_BIN") {
        let path = PathBuf::from(override_path);
        return path.is_file().then_some(vec![path]);
    }
    if let Some(repobin) = resolve_repobin(cwd)
        && probe_repobin_checkleft(&repobin, cwd)
    {
        return Some(vec![repobin, PathBuf::from("exec"), PathBuf::from("checkleft")]);
    }
    let candidate = cwd.join("bin").join("checkleft");
    if candidate.is_file() {
        return Some(vec![candidate]);
    }
    which::which("checkleft").ok().map(|p| vec![p])
}

/// Resolve the `repobin` binary: `<cwd>/bin/repobin`, then a PATH search.
fn resolve_repobin(cwd: &Path) -> Option<PathBuf> {
    let candidate = cwd.join("bin").join("repobin");
    if candidate.is_file() {
        return Some(candidate);
    }
    which::which("repobin").ok()
}

/// Confirm `repobin exec checkleft` can actually dispatch before the gate
/// trusts it for the real `run` invocation.
///
/// `repobin exec checkleft` builds checkleft with `bazel build` on a
/// dispatch-cache miss. If that build fails (a broken crate elsewhere in
/// the tree, a bazel toolchain problem, a missing `REPOBIN.toml` entry),
/// `repobin exec checkleft run` itself exits non-zero with nothing on
/// stdout — indistinguishable, from the gate's point of view, from a
/// checkleft "internal error" — which would hard-block every push with no
/// `BYPASS_` escape, for a failure that has nothing to do with the change
/// being pushed. `--version` is cheap (no `CHECKS.yaml` / VCS context
/// needed) and shares the same on-disk dispatch cache as `run`, so probing
/// with it first pays the build cost at most once and never twice.
fn probe_repobin_checkleft(repobin: &Path, cwd: &Path) -> bool {
    let invocation = CommandInvocation {
        cwd: cwd.to_path_buf(),
        program: repobin.to_string_lossy().into_owned(),
        args: vec!["exec".to_string(), "checkleft".to_string(), "--version".to_string()],
        env: vec![],
    };
    match RealCommandRunner.run_with_timeout_capturing_output(&invocation, &[], CHECKLEFT_PROBE_TIMEOUT) {
        Ok(Some(output)) => output.status.success(),
        // Exec error or timeout (a wedged bazel build) -- treated as a
        // failed probe, same as a non-zero exit: resolution falls through
        // to the legacy <cwd>/bin/checkleft / PATH lookup.
        Ok(None) | Err(_) => false,
    }
}
