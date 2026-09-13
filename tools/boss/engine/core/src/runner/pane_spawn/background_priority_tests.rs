//! Tests for the worker Darwin-background-priority clause: lookup, logging
//! on failure, and that a failed de-prioritisation never blocks spawn.

use super::{
    TASKPOLICY_CANDIDATES, worker_background_priority_clause, worker_background_priority_clause_with_candidates,
};
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use tempfile::TempDir;

fn write_executable(dir: &TempDir, name: &str, body: &str) -> std::path::PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

fn run_clause(clause: &str, extra: &str) -> std::process::Output {
    Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{clause}{extra}"))
        .output()
        .expect("/bin/sh -c must run")
}

#[test]
fn default_clause_tries_known_locations_and_does_not_swallow_output() {
    let clause = worker_background_priority_clause();
    assert!(
        !clause.contains("/dev/null"),
        "swallowing taskpolicy's result is what let a wrong path persist silently: {clause}"
    );
    for candidate in TASKPOLICY_CANDIDATES {
        assert!(clause.contains(candidate), "clause must try {candidate}, got: {clause}");
    }
    assert!(
        clause.contains("command -v taskpolicy"),
        "clause must fall back to PATH after known locations, got: {clause}"
    );
    assert!(
        clause.contains("exited $?"),
        "non-zero taskpolicy exit must be logged with the status: {clause}"
    );
    assert!(
        clause.contains("not found at candidate paths or on PATH"),
        "a missing binary must be logged, got: {clause}"
    );
    assert_eq!(
        TASKPOLICY_CANDIDATES,
        &["/usr/sbin/taskpolicy", "/usr/bin/taskpolicy"],
        "do not hardcode a single OS-version-specific path"
    );
}

#[test]
fn nonzero_taskpolicy_exit_is_logged_and_the_script_continues() {
    let dir = TempDir::new().unwrap();
    let fake = write_executable(&dir, "taskpolicy", "#!/bin/sh\necho mock-stderr >&2\nexit 17\n");
    let clause = worker_background_priority_clause_with_candidates(&[fake.to_str().unwrap()]);
    let output = run_clause(&clause, "echo STILL_RUNNING\n");
    assert!(
        output.status.success(),
        "de-prioritisation failure must not abort the spawn script: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "macos")]
    {
        assert!(
            stderr.contains("exited 17"),
            "stderr must name the exit status, got: {stderr:?}"
        );
        assert!(
            stderr.contains(fake.to_str().unwrap()),
            "stderr must name the path attempted, got: {stderr:?}"
        );
        assert!(
            stderr.contains("mock-stderr"),
            "stderr must keep taskpolicy's own output, got: {stderr:?}"
        );
    }
    assert!(
        stdout.contains("STILL_RUNNING"),
        "commands after a failed de-prioritisation must still run, stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn missing_taskpolicy_is_logged_and_the_script_continues() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-such-taskpolicy");
    // The Darwin guard uses `/usr/bin/uname` by absolute path. PATH is
    // restricted so `command -v taskpolicy` cannot find a host binary.
    let clause = worker_background_priority_clause_with_candidates(&[missing.to_str().unwrap()]);
    let output = Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("{clause}echo STILL_RUNNING\n"))
        .env("PATH", dir.path())
        .output()
        .expect("/bin/sh -c must run");
    assert!(
        output.status.success(),
        "a missing taskpolicy must not abort the spawn script: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    #[cfg(target_os = "macos")]
    {
        assert!(
            stderr.contains("not found at candidate paths or on PATH"),
            "stderr must say lookup failed, got: {stderr:?}"
        );
    }
    assert!(
        stdout.contains("STILL_RUNNING"),
        "commands after a failed lookup must still run, stdout={stdout:?} stderr={stderr:?}"
    );
}
