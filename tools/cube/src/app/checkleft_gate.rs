//! The pre-push `checkleft` gate: locate the binary, run it, and refuse the
//! push when it reports findings.

use std::path::{Path, PathBuf};

use crate::app::errors::{CubeError, Result};
use crate::app::stage::run_stage;

/// Run the repository's checkleft against the outgoing changes before a
/// push, refusing the push when checkleft reports errors.
///
/// This is the ergonomic, sanctioned-flow half of the "run checkleft
/// before every PR push" guard — the same enforcement the Boss runtime
/// applies to raw `jj git push` is applied here for `cube pr create` /
/// `cube pr update`. Single source of truth: it shells out to checkleft and
/// trusts its exit code (0 = clean, non-zero = errors) — no policy logic
/// is duplicated. checkleft's own "no CHECKS.yaml → exit 0" behaviour
/// means repos without convention checks pass transparently.
///
/// Bypass is checkleft's own `BYPASS_<CHECK>=<reason>` directives in the
/// commit message / PR description; there is no separate cube-level
/// override.
///
/// Fail-open by construction: when no checkleft binary is found the gate
/// is a no-op, but a clear warning is emitted to stderr so the skip is
/// visible rather than silent. The only refusal is a checkleft that
/// actually reported errors. Resolution order: see [`resolve_checkleft_bin`].
pub(super) fn run_checkleft_gate(cwd: &Path) -> Result<()> {
    run_stage("running checkleft push-gate", || {
        run_checkleft_gate_impl(cwd, resolve_checkleft_bin(cwd))
    })
}

/// Inner implementation: runs `checkleft run` using the given binary, or
/// emits a skip warning and returns `Ok(())` when `checkleft` is `None`.
/// Separated from [`run_checkleft_gate`] so tests can inject a pre-resolved
/// binary without modifying global PATH.
pub(super) fn run_checkleft_gate_impl(cwd: &Path, checkleft: Option<PathBuf>) -> Result<()> {
    let Some(checkleft) = checkleft else {
        eprintln!(
            "cube: checkleft not found via CUBE_CHECKLEFT_BIN, {}/bin/checkleft, or PATH \
             — push gate SKIPPED",
            cwd.display()
        );
        return Ok(());
    };

    let output = std::process::Command::new(&checkleft)
        .arg("run")
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        // Could not execute checkleft at all — fail open rather than block
        // a push on an infrastructure problem unrelated to the change.
        Err(_) => return Ok(()),
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

/// Resolve the checkleft binary to run for the push gate. Returns `None`
/// when no checkleft is available (the gate then no-ops with a warning).
/// Resolution order mirrors the Layer-1 push-guard resolver:
///   1. `CUBE_CHECKLEFT_BIN` env override (explicit path)
///   2. `<cwd>/bin/checkleft` (repobin-installed artifact)
///   3. `checkleft` on PATH (installed globally or via PATH-based repobin)
pub(super) fn resolve_checkleft_bin(cwd: &Path) -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("CUBE_CHECKLEFT_BIN") {
        let path = PathBuf::from(override_path);
        return path.is_file().then_some(path);
    }
    let candidate = cwd.join("bin").join("checkleft");
    if candidate.is_file() {
        return Some(candidate);
    }
    which::which("checkleft").ok()
}
