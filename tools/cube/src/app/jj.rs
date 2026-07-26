//! The `jj` invocation layer: timeouts, network retry, stale-working-copy
//! recovery, and the small query helpers built on top.

use std::path::Path;
use std::time::{Duration, Instant};
use std::{fs, io};

use crate::command_runner::{CommandInvocation, CommandRunner, RealCommandRunner};
use crate::{audit, paths};

use crate::app::errors::{
    CubeError, JJ_NO_JJ_REPO_SIGNATURE, JJ_OP_DIVERGED_SIGNATURE, JJ_STALE_OP_SIGNATURE, JJ_STALE_SIGNATURE, Result,
};

#[derive(Debug, Clone)]
pub(super) struct ChangeIdentity {
    pub(super) jj_change_id: String,
    pub(super) head_commit: String,
}

/// Default per-attempt wall-clock bound for any subprocess cube spawns
/// through [`run_jj_network`] / [`run_jj`]. Generous enough that a slow but
/// live `jj git fetch` of a large repo completes, tight enough that a
/// wedged half-open ssh connection is killed in minutes rather than the
/// 16+ the unbounded path was observed to hang. Overridable via
/// `CUBE_NETWORK_TIMEOUT_SECS` for hosts with unusual repos or links.
const DEFAULT_NETWORK_CMD_TIMEOUT_SECS: u64 = 120;

/// How many extra times a read-only network op (fetch / clone / `gh` /
/// `ls-remote`) is retried after a timeout or a transient network failure
/// before the error is surfaced.
const NETWORK_CMD_RETRIES: u32 = 2;

/// Resolve the per-attempt network command timeout, honouring the
/// `CUBE_NETWORK_TIMEOUT_SECS` override (clamped to a sane floor so an
/// operator typo can't reintroduce a near-zero/no timeout).
pub(super) fn network_cmd_timeout() -> Duration {
    let secs = std::env::var("CUBE_NETWORK_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s >= 5)
        .unwrap_or(DEFAULT_NETWORK_CMD_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Default wall-clock bound for a single `jj git push` attempt. Larger than
/// [`DEFAULT_NETWORK_CMD_TIMEOUT_SECS`] on purpose: a push against a store
/// shared by many concurrent cube workspaces can legitimately queue for
/// minutes behind the fleet's other jj operations (fetches, snapshots,
/// `jj new`) serializing on the same on-disk op-log lock, not because
/// anything is actually wedged. Killing and restarting the git transport
/// every ~2 minutes (as [`DEFAULT_NETWORK_CMD_TIMEOUT_SECS`] would) throws
/// away queueing progress already made; one longer attempt with heartbeats
/// (see [`crate::command_runner::RealCommandRunner::run_with_timeout`]) is
/// cheaper and matches the multi-minute waits observed in production.
/// Overridable via `CUBE_PUSH_TIMEOUT_SECS`.
const DEFAULT_PUSH_CMD_TIMEOUT_SECS: u64 = 300;

/// How many extra times a `jj git push` is retried after a timeout or a
/// transient network failure before the error is surfaced. Safe to retry:
/// every push this wraps is a non-force `--allow-new` push of an already-
/// committed bookmark, so retrying an attempt that actually landed just
/// reconfirms the remote ref is already where we want it.
const PUSH_CMD_RETRIES: u32 = 1;

/// Resolve the per-attempt `jj git push` timeout, honouring the
/// `CUBE_PUSH_TIMEOUT_SECS` override (clamped to a sane floor so an
/// operator typo can't reintroduce a near-zero/no timeout).
fn push_cmd_timeout() -> Duration {
    let secs = std::env::var("CUBE_PUSH_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s >= 5)
        .unwrap_or(DEFAULT_PUSH_CMD_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Run `jj git push` with a bounded per-attempt deadline and a bounded
/// retry on timeout or transient network failure. No silent crawl: every
/// attempt goes through [`CommandRunner::run_with_timeout`], which emits
/// its own stderr heartbeat while the child is still running, so a push
/// queued behind shared-store contention is never quiet for longer than
/// that heartbeat interval — a caller's own output-silence timeout (e.g. a
/// Bash tool's default) sees progress instead of reading silence as a hang.
///
/// Deliberately not [`run_jj`] / [`run_jj_network`]: those apply
/// stale-working-copy and colocate-init recovery aimed at read/update
/// operations on the local working copy, which doesn't fit a push (the
/// invocation already carries `--ignore-working-copy`, so there is no
/// working copy state to recover), and they use a shorter timeout tuned for
/// fetches rather than a lock-contended push.
pub(super) fn run_jj_push(runner: &dyn CommandRunner, invocation: &CommandInvocation) -> Result<String> {
    let timeout = push_cmd_timeout();
    let mut attempt: u32 = 0;
    loop {
        match runner.run_with_timeout(invocation, timeout) {
            Ok(out) => return Ok(out),
            Err(err) if attempt < PUSH_CMD_RETRIES && is_retryable_network_error(&err) => {
                attempt += 1;
                eprintln!(
                    "cube: `{} {}` did not complete within {}s (attempt {attempt}/{PUSH_CMD_RETRIES}); \
                     this usually means the shared jj store is contended by concurrent workspaces — \
                     retrying: {err}",
                    invocation.program,
                    invocation.args.join(" "),
                    timeout.as_secs(),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

/// Stable substrings that mark a network failure as transient (worth a
/// bounded retry) rather than a hard error like an auth or merge failure.
/// Matched case-insensitively against a failed command's stderr.
const TRANSIENT_NETWORK_SIGNATURES: &[&str] = &[
    "connection reset",
    "connection timed out",
    "connection refused",
    "could not resolve",
    "temporary failure in name resolution",
    "network is unreachable",
    "operation timed out",
    "timed out",
    "early eof",
    "broken pipe",
    "ssh: connect to host",
];

/// True when `err` represents a transient network condition that a bounded
/// retry might clear: a cube-side timeout, or a command failure whose
/// stderr matches a known-transient signature.
pub(super) fn is_retryable_network_error(err: &CubeError) -> bool {
    match err {
        CubeError::CommandTimedOut { .. } => true,
        CubeError::CommandFailed { stderr, .. } => {
            let lowered = stderr.to_ascii_lowercase();
            TRANSIENT_NETWORK_SIGNATURES.iter().any(|sig| lowered.contains(sig))
        }
        _ => false,
    }
}

/// How long a subprocess started *right now* may run, given a caller's
/// wall-clock `deadline` and the operation's own `default` per-attempt bound:
/// the smaller of the two. `None` means the deadline has already passed and no
/// new subprocess should be started at all.
///
/// This is what makes a caller's time budget mean something. Probe *count* was
/// never a bound on probe *cost*: one `jj git fetch` against a slow remote is
/// [`DEFAULT_NETWORK_CMD_TIMEOUT_SECS`] (120s) per attempt with
/// [`NETWORK_CMD_RETRIES`] retries behind it, so a single candidate could
/// outlast a 20-second GC budget — and the engine's 90-second lease timeout —
/// several times over while the pass believed itself bounded.
pub(super) fn budgeted_timeout(deadline: Option<Instant>, default: Duration) -> Option<Duration> {
    let Some(deadline) = deadline else {
        return Some(default);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        None
    } else {
        Some(default.min(remaining))
    }
}

fn deadline_exceeded(invocation: &CommandInvocation) -> CubeError {
    CubeError::DeadlineExceeded {
        program: invocation.program.clone(),
        args: invocation.args.clone(),
    }
}

/// [`run_jj`] for a network operation (e.g. `jj git fetch`): the same
/// recovery behaviour, plus a bounded retry on a timeout or transient
/// network failure. A non-transient failure (auth, conflict, bad revset)
/// returns immediately. This is the wrapper the lease/release reset paths
/// use so a flaky-but-alive remote self-heals while a genuinely wedged one
/// is bounded by [`network_cmd_timeout`] rather than hanging forever.
pub(super) fn run_jj_network(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    invocation: &CommandInvocation,
) -> Result<String> {
    run_jj_network_within(runner, database_path, invocation, None)
}

/// [`run_jj_network`] under a caller's wall-clock `deadline`: every attempt is
/// bounded by whatever the deadline has left (never more than
/// [`network_cmd_timeout`]), and no retry is started once it has passed.
pub(super) fn run_jj_network_within(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    invocation: &CommandInvocation,
    deadline: Option<Instant>,
) -> Result<String> {
    let mut attempt: u32 = 0;
    loop {
        match run_jj_within(runner, database_path, invocation, deadline) {
            Ok(out) => return Ok(out),
            // A retry that cannot fit inside the caller's remaining budget is
            // not a retry, it is an overrun: surface the failure instead.
            Err(err)
                if attempt < NETWORK_CMD_RETRIES
                    && is_retryable_network_error(&err)
                    && budgeted_timeout(deadline, network_cmd_timeout()).is_some() =>
            {
                attempt += 1;
                eprintln!(
                    "cube: network command `{} {}` failed transiently (attempt {attempt}/{NETWORK_CMD_RETRIES}); retrying: {err}",
                    invocation.program,
                    invocation.args.join(" "),
                );
                audit!(
                    database_path,
                    "workspace.network_retry",
                    workspace_path = invocation.cwd.display().to_string(),
                    program = invocation.program,
                    args = invocation.args,
                    attempt = attempt,
                    error = err.to_string(),
                );
            }
            Err(err) => return Err(err),
        }
    }
}

/// Run a `jj` command against a workspace, transparently recovering
/// from a stale working copy, op-log divergence, or a missing jj repo
/// alongside an existing git repo. If the underlying command fails with
/// `working copy is stale` or `seems to be a sibling`, runs
/// `jj workspace update-stale` once and retries. If it fails with
/// `there is no jj repo` and a `.git/` directory is present, runs
/// `jj git init --colocate` once and retries. If it fails with
/// `there is no jj repo` and neither `.git/` nor `.jj/` is present,
/// surfaces a clear `NoAvailableWorkspace` error naming the broken
/// workspace path instead of the raw jj message. Other failures and
/// non-`jj` invocations pass through untouched.
///
/// Every attempt is bounded by [`network_cmd_timeout`] so a wedged
/// subprocess (most importantly a half-open `jj git fetch`) is killed
/// rather than hanging cube — and, critically, any lock cube holds is
/// released instead of starving the whole repo pool.
pub(super) fn run_jj(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    invocation: &CommandInvocation,
) -> Result<String> {
    run_jj_within(runner, database_path, invocation, None)
}

/// [`run_jj`] under a caller's wall-clock `deadline`. Every subprocess this
/// spawns — the invocation itself and each recovery retry — is bounded by
/// whatever the deadline has left, and a deadline that has already passed
/// yields [`CubeError::DeadlineExceeded`] without spawning anything.
pub(super) fn run_jj_within(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    invocation: &CommandInvocation,
    deadline: Option<Instant>,
) -> Result<String> {
    let Some(timeout) = budgeted_timeout(deadline, network_cmd_timeout()) else {
        return Err(deadline_exceeded(invocation));
    };
    match runner.run_with_timeout(invocation, timeout) {
        Ok(out) => Ok(out),
        Err(err) => {
            // Sibling heal: workspace has .git but no .jj — colocate-init jj.
            if jj_needs_colocate_init(&err, &invocation.cwd) {
                eprintln!(
                    "cube: initialised jj on existing git workspace {}",
                    invocation.cwd.display()
                );
                let init = RealCommandRunner::invocation(&invocation.cwd, "jj", &["git", "init", "--colocate"]);
                if runner.run(&init).is_err() {
                    return Err(err);
                }
                audit!(
                    database_path,
                    "workspace.jj_colocate_initialised",
                    workspace_path = invocation.cwd.display().to_string(),
                    program = invocation.program,
                    args = invocation.args,
                );
                let Some(retry_timeout) = budgeted_timeout(deadline, network_cmd_timeout()) else {
                    return Err(err);
                };
                return match runner.run_with_timeout(invocation, retry_timeout) {
                    Ok(out) => Ok(out),
                    Err(_) => Err(err),
                };
            }

            // Broken-empty: workspace has neither .jj/ nor .git/ — the
            // directory was likely wiped externally. Surface a clear error
            // naming the path and what's missing rather than the raw jj
            // "no jj repo" message, which gives no actionable information.
            if jj_workspace_broken_empty(&err, &invocation.cwd) {
                return Err(CubeError::NoAvailableWorkspace(format!(
                    "workspace at `{}` has neither .jj/ nor .git/ (broken-empty): \
                     the workspace directory exists but no jj or git repository was found. \
                     Re-clone manually or `cube workspace force-release` and retry.",
                    invocation.cwd.display()
                )));
            }

            let Some(recovery_kind) = jj_update_stale_recovery_kind(&err) else {
                return Err(err);
            };
            if recovery_kind == "workspace.op_diverged_recovered" {
                eprintln!(
                    "cube: jj op-log diverged on {}; running `jj workspace update-stale` to recover",
                    invocation.cwd.display()
                );
            }
            let update_stale = RealCommandRunner::invocation(&invocation.cwd, "jj", &["workspace", "update-stale"]);
            if let Err(update_err) = runner.run(&update_stale) {
                return Err(CubeError::StaleRecoveryFailed {
                    workspace_path: invocation.cwd.clone(),
                    cause: format!("jj workspace update-stale failed: {update_err}"),
                });
            }
            audit!(
                database_path,
                recovery_kind,
                workspace_path = invocation.cwd.display().to_string(),
                program = invocation.program,
                args = invocation.args,
            );
            let Some(retry_timeout) = budgeted_timeout(deadline, network_cmd_timeout()) else {
                return Err(deadline_exceeded(invocation));
            };
            match runner.run_with_timeout(invocation, retry_timeout) {
                Ok(out) => Ok(out),
                Err(retry_err) => Err(CubeError::StaleRecoveryFailed {
                    workspace_path: invocation.cwd.clone(),
                    cause: format!("retry after update-stale failed: {retry_err}"),
                }),
            }
        }
    }
}

/// Returns `true` when the error is `jj`'s "no jj repo" diagnostic AND a
/// `.git/` directory exists at `cwd`, meaning `jj git init --colocate` can
/// recover the workspace. Returns `false` for all other errors or when
/// `.git/` is absent (truly broken state — do not paper over it).
pub(super) fn jj_needs_colocate_init(err: &CubeError, cwd: &Path) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains(JJ_NO_JJ_REPO_SIGNATURE) && cwd.join(".git").is_dir()
}

/// Returns `true` when the error is `jj`'s "no jj repo" diagnostic AND
/// neither `.jj/` nor `.git/` exists at `cwd`. This is the shorter error
/// variant jj emits when the directory has no repo at all (as opposed to
/// the longer hint-bearing form jj emits when `.git/` is present without
/// `.jj/`). Directory state is checked directly rather than by inspecting
/// jj's error text — the text is brittle; the directory check is not.
fn jj_workspace_broken_empty(err: &CubeError, cwd: &Path) -> bool {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return false;
    };
    if program != "jj" {
        return false;
    }
    let lower = stderr.to_lowercase();
    lower.contains(JJ_NO_JJ_REPO_SIGNATURE) && !cwd.join(".jj").is_dir() && !cwd.join(".git").is_dir()
}

/// Returns the audit event name if the error is one that `jj workspace
/// update-stale` can fix, or `None` if the error is unrelated.
pub(super) fn jj_update_stale_recovery_kind(err: &CubeError) -> Option<&'static str> {
    let CubeError::CommandFailed { program, stderr, .. } = err else {
        return None;
    };
    if program != "jj" {
        return None;
    }
    let lower = stderr.to_lowercase();
    if lower.contains(JJ_STALE_SIGNATURE) || lower.contains(JJ_STALE_OP_SIGNATURE) {
        return Some("workspace.stale_recovered");
    }
    if lower.contains(JJ_OP_DIVERGED_SIGNATURE) {
        return Some("workspace.op_diverged_recovered");
    }
    None
}

pub(super) fn current_workspace_commit(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<String> {
    run_jj(
        runner,
        database_path,
        &CommandInvocation {
            cwd: workspace_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "log".to_string(),
                "--no-graph".to_string(),
                "-r".to_string(),
                "@".to_string(),
                "-T".to_string(),
                "commit_id.short()".to_string(),
            ],
            env: vec![],
        },
    )
}

pub(super) fn current_change_identity(
    runner: &dyn CommandRunner,
    database_path: Option<&Path>,
    workspace_path: &Path,
) -> Result<ChangeIdentity> {
    let output = run_jj(
        runner,
        database_path,
        &CommandInvocation {
            cwd: workspace_path.to_path_buf(),
            program: "jj".to_string(),
            args: vec![
                "log".to_string(),
                "--no-graph".to_string(),
                "-r".to_string(),
                "@".to_string(),
                "-T".to_string(),
                "change_id ++ \"\\n\" ++ commit_id.short()".to_string(),
            ],
            env: vec![],
        },
    )?;
    let mut lines = output.lines().map(str::trim).filter(|line| !line.is_empty());
    let jj_change_id = lines
        .next()
        .ok_or_else(|| CubeError::InvalidArgument("jj change query did not return a change id".to_string()))?
        .to_string();
    let head_commit = lines
        .next()
        .ok_or_else(|| CubeError::InvalidArgument("jj change query did not return a head commit".to_string()))?
        .to_string();
    Ok(ChangeIdentity {
        jj_change_id,
        head_commit,
    })
}

pub(super) fn workspace_path_exists(record: &crate::metadata::WorkspaceRecord) -> bool {
    record.workspace_path.is_dir()
}

pub(super) fn cleanup_workspace_logs(workspace_id: &str) -> Result<()> {
    if let Ok(logs_path) = paths::workspace_logs_path(workspace_id) {
        match fs::remove_dir_all(&logs_path) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(CubeError::Io(err)),
        }
    }
    Ok(())
}
