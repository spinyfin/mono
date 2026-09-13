//! Tests for the worker Darwin-background-priority clause: lookup, logging
//! on failure, and that a failed de-prioritisation never blocks spawn.

use super::{
    TASKPOLICY_CANDIDATES, maybe_warn_taskpolicy_host, taskpolicy_known_location, worker_background_priority_clause,
    worker_background_priority_clause_with_candidates,
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

#[test]
fn known_location_is_none_when_no_candidate_exists() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-such-taskpolicy");
    assert_eq!(
        taskpolicy_known_location(&[missing.to_str().unwrap()]),
        None,
        "a path that is not a file must not count as resolved"
    );
}

#[test]
fn known_location_returns_the_first_existing_candidate() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("missing");
    let present = dir.path().join("present");
    std::fs::write(&present, b"").unwrap();
    let later = dir.path().join("later");
    std::fs::write(&later, b"").unwrap();
    assert_eq!(
        taskpolicy_known_location(&[
            missing.to_str().unwrap(),
            present.to_str().unwrap(),
            later.to_str().unwrap()
        ]),
        Some(present.to_str().unwrap()),
        "must pick the first existing candidate, not a later one"
    );
}

#[test]
fn compose_time_unresolved_taskpolicy_emits_engine_warn() {
    let buffer = crate::test_support::log_capture::install();
    let start = buffer.lock().len();
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("compose-time-missing-taskpolicy");
    let needle = missing.to_str().unwrap();
    maybe_warn_taskpolicy_host(&[needle]);
    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let line = captured
        .lines()
        .find(|line| line.contains(needle) && line.contains("not found at known locations"))
        .unwrap_or_else(|| panic!("no unresolved-taskpolicy warn; captured: {captured}"));
    assert!(
        line.contains("WARN"),
        "missing taskpolicy at compose time must be a warning: {line}"
    );
}

#[test]
fn compose_time_nonzero_taskpolicy_exit_emits_engine_warn() {
    let buffer = crate::test_support::log_capture::install();
    let start = buffer.lock().len();
    let dir = TempDir::new().unwrap();
    let fake = write_executable(
        &dir,
        "failing-taskpolicy",
        "#!/bin/sh\necho probe-stderr >&2\nexit 17\n",
    );
    let needle = fake.to_str().unwrap();
    maybe_warn_taskpolicy_host(&[needle]);
    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let line = captured
        .lines()
        .find(|line| line.contains(needle) && line.contains("failed to set Darwin background priority"))
        .unwrap_or_else(|| panic!("no present-but-failing taskpolicy warn; captured: {captured}"));
    assert!(
        line.contains("WARN"),
        "a resolved-but-failing taskpolicy must be a warning: {line}"
    );
    assert!(line.contains("17"), "engine warn must name the exit status: {line}");
    assert!(
        line.contains("probe-stderr"),
        "engine warn must keep taskpolicy's own output: {line}"
    );
}

#[test]
fn compose_time_successful_taskpolicy_probe_does_not_warn() {
    let buffer = crate::test_support::log_capture::install();
    let start = buffer.lock().len();
    let dir = TempDir::new().unwrap();
    let fake = write_executable(&dir, "ok-taskpolicy", "#!/bin/sh\nexit 0\n");
    let needle = fake.to_str().unwrap();
    maybe_warn_taskpolicy_host(&[needle]);
    let captured = String::from_utf8(buffer.lock()[start..].to_vec()).expect("utf8 log capture");
    let ours: Vec<&str> = captured.lines().filter(|line| line.contains(needle)).collect();
    assert!(
        ours.iter()
            .all(|line| !line.contains("WARN") && !line.contains("ERROR")),
        "a successful probe must not warn; lines: {ours:#?}"
    );
}
