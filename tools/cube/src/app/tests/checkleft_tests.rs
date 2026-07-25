use tempfile::TempDir;

use super::support::ENV_MUTEX;
use crate::app::checkleft_gate::{resolve_checkleft_bin, run_checkleft_gate, run_checkleft_gate_impl};
use crate::app::errors::CubeError;

/// Write an executable fake `checkleft` at `<root>/bin/checkleft` that
/// prints `stdout` and exits with `exit_code`.
fn write_fake_checkleft(root: &std::path::Path, exit_code: i32, stdout: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("checkleft");
    let stdout_escaped = stdout.replace('\'', "'\\''");
    let script = format!("#!/bin/sh\nprintf '%s\\n' '{stdout_escaped}'\nexit {exit_code}\n");
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// Write an executable fake `checkleft` into `<root>/bin/checkleft` that
/// produces nothing on stdout and `stderr_msg` on stderr, then exits with
/// `exit_code`. Models a parser/internal crash where checkleft emits an
/// error to stderr without printing any findings to stdout.
fn write_fake_checkleft_stderr_only(root: &std::path::Path, exit_code: i32, stderr_msg: &str) {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("checkleft");
    let script = format!("#!/bin/sh\necho '{stderr_msg}' >&2\nexit {exit_code}\n");
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// Write an executable fake `checkleft` directly inside `dir` (not in a `bin/`
/// subdirectory) so it can be placed on PATH without being the `bin/checkleft`
/// repobin-artifact path.
fn write_fake_checkleft_to_dir(dir: &std::path::Path, exit_code: i32, stdout: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("checkleft");
    let stdout_escaped = stdout.replace('\'', "'\\''");
    let script = format!("#!/bin/sh\nprintf '%s\\n' '{stdout_escaped}'\nexit {exit_code}\n");
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// RAII guard that saves PATH and CUBE_CHECKLEFT_BIN on construction and
/// restores them on drop. Always acquire `ENV_MUTEX` first.
pub(super) struct CheckleftEnvGuard {
    pub(super) orig_path: Option<std::ffi::OsString>,
    pub(super) orig_cube_bin: Option<std::ffi::OsString>,
}

impl CheckleftEnvGuard {
    pub(super) fn with_path(new_path: &std::ffi::OsStr) -> Self {
        let orig_path = std::env::var_os("PATH");
        let orig_cube_bin = std::env::var_os("CUBE_CHECKLEFT_BIN");
        unsafe {
            std::env::set_var("PATH", new_path);
            std::env::remove_var("CUBE_CHECKLEFT_BIN");
        }
        CheckleftEnvGuard {
            orig_path,
            orig_cube_bin,
        }
    }

    // Sets CUBE_CHECKLEFT_BIN to a nonexistent path so resolve_checkleft_bin
    // returns None (gate is a no-op) without modifying PATH. Use in tests that
    // call ensure_pr / run_with_dependencies but don't want to test the gate
    // itself. Always hold ENV_MUTEX before calling this.
    pub(super) fn with_gate_disabled() -> Self {
        let orig_path = std::env::var_os("PATH");
        let orig_cube_bin = std::env::var_os("CUBE_CHECKLEFT_BIN");
        unsafe {
            std::env::set_var("CUBE_CHECKLEFT_BIN", "");
        }
        CheckleftEnvGuard {
            orig_path,
            orig_cube_bin,
        }
    }
}

impl Drop for CheckleftEnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.orig_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
            // Restore CUBE_CHECKLEFT_BIN to its original state, including
            // removing it if it was absent before (e.g. after with_gate_disabled).
            match &self.orig_cube_bin {
                Some(v) => std::env::set_var("CUBE_CHECKLEFT_BIN", v),
                None => std::env::remove_var("CUBE_CHECKLEFT_BIN"),
            }
        }
    }
}

#[test]
fn checkleft_gate_is_skipped_when_no_checkleft_anywhere() {
    // When there is no bin/checkleft, no CUBE_CHECKLEFT_BIN, and no
    // checkleft on PATH, the gate must emit a warning and proceed fail-open.
    let dir = TempDir::new().unwrap();
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_path(std::ffi::OsStr::new(""));
    assert!(
        run_checkleft_gate(dir.path(), None).is_ok(),
        "gate must be a no-op when no checkleft binary is present anywhere",
    );
}

#[test]
fn checkleft_gate_proceeds_when_checkleft_clean() {
    let dir = TempDir::new().unwrap();
    write_fake_checkleft(dir.path(), 0, "checks: no findings");
    let checkleft = Some(dir.path().join("bin").join("checkleft"));
    assert!(
        run_checkleft_gate_impl(dir.path(), checkleft, None).is_ok(),
        "gate must proceed when checkleft exits 0",
    );
}

#[test]
fn checkleft_gate_refuses_with_findings_when_checkleft_fails() {
    let dir = TempDir::new().unwrap();
    write_fake_checkleft(dir.path(), 1, "error[rustfmt]: file needs formatting");
    let checkleft = Some(dir.path().join("bin").join("checkleft"));
    let err = run_checkleft_gate_impl(dir.path(), checkleft, None)
        .expect_err("gate must refuse when checkleft exits non-zero");
    let CubeError::InvalidArgument(msg) = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(msg.contains("error[rustfmt]"), "refusal must echo the findings: {msg}");
    assert!(msg.contains("BYPASS_"), "refusal must include bypass guidance: {msg}");
}

#[test]
fn checkleft_gate_uses_path_fallback_and_blocks_on_errors() {
    // Regression test: when bin/checkleft is absent and CUBE_CHECKLEFT_BIN is
    // not set, the gate must find checkleft on PATH and block when that binary
    // reports errors. This covers the cube workspace case where repobin-install
    // has not run but checkleft is globally available (e.g. via ~/bin).
    //
    // To avoid leaking PATH mutations to concurrently-running tests, we:
    //   1. Hold ENV_MUTEX for only as long as it takes to resolve the binary.
    //   2. Call run_checkleft_gate_impl directly with the pre-resolved binary;
    //      no PATH modification escapes beyond the resolve step.
    let workspace = TempDir::new().unwrap();
    let path_dir = TempDir::new().unwrap();
    write_fake_checkleft_to_dir(path_dir.path(), 1, "error[rustfmt]: file needs formatting");

    // Briefly acquire the lock, prepend path_dir to PATH, resolve the binary,
    // then release the lock (CheckleftEnvGuard restores PATH on drop).
    let resolved = {
        let _lock = ENV_MUTEX.lock().unwrap();
        let new_path = std::env::join_paths(
            std::iter::once(path_dir.path().to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let _env = CheckleftEnvGuard::with_path(&new_path);
        // workspace has no bin/checkleft; CUBE_CHECKLEFT_BIN is cleared by guard.
        // So resolve_checkleft_bin must fall through to the PATH fallback.
        resolve_checkleft_bin(workspace.path())
    }; // lock released and PATH restored here

    assert_eq!(
        resolved.as_deref(),
        Some(path_dir.path().join("checkleft").as_path()),
        "PATH fallback must resolve the fake checkleft from the prepended dir",
    );

    // Gate execution is independent of PATH; inject the resolved binary.
    let err = run_checkleft_gate_impl(workspace.path(), resolved, None)
        .expect_err("gate must block when PATH checkleft reports errors");
    let CubeError::InvalidArgument(msg) = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        msg.contains("error[rustfmt]"),
        "refusal must echo the PATH-checkleft findings: {msg}",
    );
}

#[test]
fn checkleft_gate_reports_internal_error_when_only_stderr() {
    // When checkleft exits non-zero with nothing on stdout but an error on
    // stderr (a parser/internal crash), the gate must use the "internal
    // error" message rather than "found errors that must be fixed". This
    // prevents users from thinking they have policy violations to fix.
    let dir = TempDir::new().unwrap();
    write_fake_checkleft_stderr_only(dir.path(), 1, "error: unsupported jj diff summary line: X some/file.rs");
    let checkleft = Some(dir.path().join("bin").join("checkleft"));
    let err = run_checkleft_gate_impl(dir.path(), checkleft, None)
        .expect_err("gate must block when checkleft exits non-zero");
    let CubeError::InvalidArgument(msg) = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        msg.contains("internal error"),
        "message must say 'internal error', not 'errors that must be fixed': {msg}",
    );
    assert!(
        !msg.contains("BYPASS_"),
        "internal error message must NOT include bypass guidance: {msg}",
    );
    assert!(
        msg.contains("unsupported jj diff summary line"),
        "message must include the stderr detail: {msg}",
    );
}

/// Write an executable fake `checkleft` that echoes `CHECKS_PR_DESCRIPTION`
/// (or "UNSET" when absent) to stdout, then exits 1 so the value shows up in
/// the gate's refusal message.
fn write_fake_checkleft_env_echo(root: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let path = bin.join("checkleft");
    let script = "#!/bin/sh\nprintf 'CHECKS_PR_DESCRIPTION=%s\\n' \"${CHECKS_PR_DESCRIPTION-UNSET}\"\nexit 1\n";
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

#[test]
fn checkleft_gate_exports_pr_description_to_checkleft_subprocess() {
    // The bug this closes: `cube pr create` runs the gate before the PR
    // exists, so checkleft's own env/branch-lookup resolution can't find a
    // description on its own. The gate must hand it the exact body cube is
    // about to submit via CHECKS_PR_DESCRIPTION — checkleft already honours
    // that env var at its highest precedence.
    let dir = TempDir::new().unwrap();
    write_fake_checkleft_env_echo(dir.path());
    let checkleft = Some(dir.path().join("bin").join("checkleft"));
    let err = run_checkleft_gate_impl(dir.path(), checkleft, Some("PR body mentioning T1234"))
        .expect_err("fake checkleft always exits 1");
    let CubeError::InvalidArgument(msg) = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        msg.contains("CHECKS_PR_DESCRIPTION=PR body mentioning T1234"),
        "checkleft subprocess must see the resolved PR body via CHECKS_PR_DESCRIPTION: {msg}",
    );
}

#[test]
fn checkleft_gate_leaves_pr_description_unset_when_none() {
    // Callers with no PR body to check (cube pr update / pr push) must not
    // fabricate one — checkleft falls through to its own branch/PR-number
    // resolution in that case. Deliberately set an ambient
    // CHECKS_PR_DESCRIPTION in cube's own process env first: `None` must
    // actively clear it for the subprocess, not merely skip setting it —
    // otherwise the subprocess would inherit this stale ambient value.
    let _lock = ENV_MUTEX.lock().unwrap();
    let prior = std::env::var("CHECKS_PR_DESCRIPTION").ok();
    // SAFETY: ENV_MUTEX is held for the duration of this mutation and its
    // restoration below, serializing this against every other test that
    // touches process env.
    unsafe {
        std::env::set_var("CHECKS_PR_DESCRIPTION", "stale ambient value that must not leak");
    }

    let dir = TempDir::new().unwrap();
    write_fake_checkleft_env_echo(dir.path());
    let checkleft = Some(dir.path().join("bin").join("checkleft"));
    let result = run_checkleft_gate_impl(dir.path(), checkleft, None);

    // SAFETY: see above.
    unsafe {
        match prior {
            Some(v) => std::env::set_var("CHECKS_PR_DESCRIPTION", v),
            None => std::env::remove_var("CHECKS_PR_DESCRIPTION"),
        }
    }

    let err = result.expect_err("fake checkleft always exits 1");
    let CubeError::InvalidArgument(msg) = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        msg.contains("CHECKS_PR_DESCRIPTION=UNSET"),
        "gate must clear an ambient CHECKS_PR_DESCRIPTION when no PR body was given, not \
         inherit it: {msg}",
    );
}
