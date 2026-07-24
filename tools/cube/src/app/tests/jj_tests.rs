use std::path::PathBuf;

use super::support::{ExpectedCommand, FakeRunner};

use crate::command_runner::RealCommandRunner;

use crate::app::errors::CubeError;
use crate::app::jj::{is_retryable_network_error, run_jj_push};

#[test]
fn is_retryable_network_error_classifies_transient_failures() {
    assert!(is_retryable_network_error(&CubeError::CommandTimedOut {
        program: "jj".to_string(),
        args: vec!["git".to_string(), "fetch".to_string()],
        timeout_secs: 120,
    }));
    assert!(is_retryable_network_error(&CubeError::CommandFailed {
        program: "jj".to_string(),
        args: vec![],
        status: Some(1),
        stderr: "ssh: connect to host github.com port 22: Connection timed out".to_string(),
    }));
    // A genuine auth/logic failure must NOT be retried.
    assert!(!is_retryable_network_error(&CubeError::CommandFailed {
        program: "jj".to_string(),
        args: vec![],
        status: Some(1),
        stderr: "fatal: permission denied (publickey)".to_string(),
    }));
}

#[test]
fn run_jj_push_retries_once_on_transient_failure_then_succeeds() {
    let cwd = std::env::current_dir().expect("cwd");
    let push_args = &[
        "git",
        "push",
        "-b",
        "my-feature",
        "--remote",
        "origin",
        "--allow-new",
        "--ignore-working-copy",
    ];
    let runner = FakeRunner::new(vec![
        ExpectedCommand {
            cwd: cwd.clone(),
            program: "jj".to_string(),
            args: push_args.iter().map(|a| (*a).to_string()).collect(),
            result: Err(CubeError::CommandTimedOut {
                program: "jj".to_string(),
                args: push_args.iter().map(|a| (*a).to_string()).collect(),
                timeout_secs: 300,
            }),
            creates_dir: None,
        },
        ExpectedCommand::ok(cwd.clone(), "jj", push_args, ""),
    ]);

    let out = run_jj_push(&runner, &RealCommandRunner::invocation(&cwd, "jj", push_args))
        .expect("should succeed after exactly one retry");
    runner.assert_exhausted();
    assert_eq!(out, "");
}

#[test]
fn run_jj_push_returns_immediately_on_non_retryable_failure() {
    let cwd = std::env::current_dir().expect("cwd");
    let push_args = &[
        "git",
        "push",
        "-b",
        "my-feature",
        "--remote",
        "origin",
        "--allow-new",
        "--ignore-working-copy",
    ];
    let runner = FakeRunner::new(vec![ExpectedCommand {
        cwd: cwd.clone(),
        program: "jj".to_string(),
        args: push_args.iter().map(|a| (*a).to_string()).collect(),
        result: Err(CubeError::CommandFailed {
            program: "jj".to_string(),
            args: push_args.iter().map(|a| (*a).to_string()).collect(),
            status: Some(1),
            stderr: "fatal: permission denied (publickey)".to_string(),
        }),
        creates_dir: None,
    }]);

    let err = run_jj_push(&runner, &RealCommandRunner::invocation(&cwd, "jj", push_args))
        .expect_err("non-retryable failure must surface immediately");
    runner.assert_exhausted();
    assert!(matches!(err, CubeError::CommandFailed { .. }));
}

#[test]
fn run_jj_propagates_non_stale_errors_unchanged() {
    // Non-stale jj failures must not trigger recovery — only the
    // specific stale signature is treated as recoverable.
    use crate::command_runner::CommandInvocation;
    let runner = FakeRunner::new(vec![ExpectedCommand {
        cwd: PathBuf::from("/tmp/ws"),
        program: "jj".to_string(),
        args: vec!["status".to_string()],
        result: Err(CubeError::CommandFailed {
            program: "jj".to_string(),
            args: vec!["status".to_string()],
            status: Some(1),
            stderr: "Error: something else entirely".to_string(),
        }),
        creates_dir: None,
    }]);

    let invocation = CommandInvocation {
        cwd: PathBuf::from("/tmp/ws"),
        program: "jj".to_string(),
        args: vec!["status".to_string()],
        env: vec![],
    };
    let err = crate::app::jj::run_jj(&runner, None, &invocation).expect_err("non-stale failure should propagate");
    runner.assert_exhausted();
    assert!(
        matches!(err, CubeError::CommandFailed { .. }),
        "expected CommandFailed, got {err:?}"
    );
}

// ───────────────────────── workspace rebase ─────────────────────────
//
// `rebase_workspace_branch` is the testable core of `cube workspace
// rebase`: deterministic boss-branch discovery, mispositioned-`@`
// self-heal, the rebase itself, and the finish-the-job advance + push.
// These tests drive it with a scripted `FakeRunner` (strict command
// sequence) so each jj/gh invocation is pinned exactly.
