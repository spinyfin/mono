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

/// Write an executable fake `repobin` under `bin_dir` that records the argv
/// it was called with (one arg per line, overwriting on each call) into
/// `argv_out`, then exits 0 with no output — modelling a clean
/// `repobin exec checkleft ...`. The gate's own resolution probes with
/// `--version` before dispatching the real `run`, so overwriting (rather
/// than appending) means `argv_out` always reflects the *last* call, i.e.
/// the real dispatch.
fn write_fake_repobin_recording_argv(bin_dir: &std::path::Path, argv_out: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin_dir).unwrap();
    let path = bin_dir.join("repobin");
    let script = format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 0\n", argv_out.display());
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
}

/// Write an executable fake `repobin` under `bin_dir` that always exits
/// non-zero, modelling a broken bazel dispatch (e.g. an unrelated crate
/// failing to build) for every subcommand, including the `--version` probe.
fn write_fake_repobin_failing(bin_dir: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(bin_dir).unwrap();
    let path = bin_dir.join("repobin");
    std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
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
    let checkleft = Some(vec![dir.path().join("bin").join("checkleft")]);
    assert!(
        run_checkleft_gate_impl(dir.path(), checkleft, None).is_ok(),
        "gate must proceed when checkleft exits 0",
    );
}

#[test]
fn checkleft_gate_refuses_with_findings_when_checkleft_fails() {
    let dir = TempDir::new().unwrap();
    write_fake_checkleft(dir.path(), 1, "error[rustfmt]: file needs formatting");
    let checkleft = Some(vec![dir.path().join("bin").join("checkleft")]);
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
        resolved,
        Some(vec![path_dir.path().join("checkleft")]),
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
    let checkleft = Some(vec![dir.path().join("bin").join("checkleft")]);
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
    let checkleft = Some(vec![dir.path().join("bin").join("checkleft")]);
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
    let checkleft = Some(vec![dir.path().join("bin").join("checkleft")]);
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

#[test]
fn checkleft_gate_prefers_repobin_exec_when_repobin_is_installed() {
    // <cwd>/bin/repobin must be preferred over a direct bin/checkleft or PATH
    // checkleft lookup, mirroring the Claude Code PreToolUse push-guard's
    // resolution order. This is the fix for rung-0 ladder pushes, which go
    // through this gate (cube workspace push), never the PreToolUse hook.
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("bin");
    let argv_out = dir.path().join("argv.txt");
    write_fake_repobin_recording_argv(&bin_dir, &argv_out);
    // Also plant a bin/checkleft that would report clean findings, to prove
    // it is never reached while a working repobin is present.
    write_fake_checkleft(
        dir.path(),
        0,
        "checks: no findings (from bin/checkleft, must not be used)",
    );

    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_path(std::ffi::OsStr::new(""));
    let resolved = resolve_checkleft_bin(dir.path());
    assert_eq!(
        resolved,
        Some(vec![bin_dir.join("repobin"), "exec".into(), "checkleft".into()]),
        "must resolve to `repobin exec checkleft` when a working repobin is installed",
    );

    let result = run_checkleft_gate_impl(dir.path(), resolved, None);
    assert!(
        result.is_ok(),
        "a clean repobin-dispatched checkleft must allow the push: {result:?}"
    );

    let argv = std::fs::read_to_string(&argv_out).expect("fake repobin must have recorded its argv");
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        vec!["exec", "checkleft", "run"],
        "gate must dispatch via `repobin exec checkleft run`: {argv:?}"
    );
}

#[test]
fn checkleft_gate_falls_back_when_repobin_dispatch_probe_fails() {
    // A repobin whose underlying bazel dispatch is broken must not hard-block
    // every push as a checkleft "internal error" with no BYPASS_ escape --
    // the gate must probe the dispatch first and fall back to the legacy
    // <cwd>/bin/checkleft resolution when the probe fails.
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("bin");
    write_fake_repobin_failing(&bin_dir);
    write_fake_checkleft(dir.path(), 1, "error[rustfmt]: file needs formatting");

    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_path(std::ffi::OsStr::new(""));
    let resolved = resolve_checkleft_bin(dir.path());
    assert_eq!(
        resolved,
        Some(vec![bin_dir.join("checkleft")]),
        "must fall back to <cwd>/bin/checkleft when the repobin dispatch probe fails",
    );

    let err = run_checkleft_gate_impl(dir.path(), resolved, None)
        .expect_err("gate must block using the fallback checkleft's findings");
    let CubeError::InvalidArgument(msg) = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert!(
        msg.contains("error[rustfmt]"),
        "block reason must come from the fallback checkleft, not a repobin dispatch error: {msg}",
    );
}

#[test]
fn checkleft_gate_finds_repobin_on_path_when_no_bin_dir_exists() {
    // The realistic cube-workspace case: nothing installs a <cwd>/bin
    // directory, so repobin must still be found via a plain PATH search.
    let workspace = TempDir::new().unwrap();
    let path_dir = TempDir::new().unwrap();
    let argv_out = workspace.path().join("argv.txt");
    write_fake_repobin_recording_argv(path_dir.path(), &argv_out);

    let resolved = {
        let _lock = ENV_MUTEX.lock().unwrap();
        let new_path = std::env::join_paths(
            std::iter::once(path_dir.path().to_path_buf())
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        let _env = CheckleftEnvGuard::with_path(&new_path);
        resolve_checkleft_bin(workspace.path())
    };

    assert_eq!(
        resolved,
        Some(vec![path_dir.path().join("repobin"), "exec".into(), "checkleft".into()]),
        "must resolve repobin found only on PATH, not just <cwd>/bin/repobin",
    );
}

#[test]
fn checkleft_gate_env_override_wins_over_a_present_repobin() {
    // CUBE_CHECKLEFT_BIN must beat repobin even when a repobin binary is
    // installed at <cwd>/bin/repobin -- the override must not be silently
    // shadowed by the repobin-preferred resolution step.
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("bin");
    let argv_out = dir.path().join("argv.txt");
    write_fake_repobin_recording_argv(&bin_dir, &argv_out);
    let override_checkleft = dir.path().join("override-checkleft");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&override_checkleft, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&override_checkleft).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&override_checkleft, perms).unwrap();
    }

    let resolved = {
        let _lock = ENV_MUTEX.lock().unwrap();
        let orig = std::env::var_os("CUBE_CHECKLEFT_BIN");
        unsafe {
            std::env::set_var("CUBE_CHECKLEFT_BIN", &override_checkleft);
        }
        let resolved = resolve_checkleft_bin(dir.path());
        unsafe {
            match &orig {
                Some(v) => std::env::set_var("CUBE_CHECKLEFT_BIN", v),
                None => std::env::remove_var("CUBE_CHECKLEFT_BIN"),
            }
        }
        resolved
    };

    assert_eq!(
        resolved,
        Some(vec![override_checkleft]),
        "CUBE_CHECKLEFT_BIN override must win over a present repobin",
    );
    assert!(
        !argv_out.exists(),
        "repobin must never be invoked once CUBE_CHECKLEFT_BIN is set",
    );
}
