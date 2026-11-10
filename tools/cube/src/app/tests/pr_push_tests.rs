use super::checkleft_tests::CheckleftEnvGuard;
use super::support::ENV_MUTEX;
use super::support::{ExpectedCommand, FakeRunner};
use clap::Parser;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;
use crate::app::errors::CubeError;

const HEAD_SHA: &str = "aabbccddeeff00112233445566778899aabbccdd";

/// GitHub's current head of `branch`: the authoritative record of where the PR is.
fn github_head_cmd(cwd: &std::path::Path, branch: &str, sha: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        cwd.to_path_buf(),
        "gh",
        &[
            "api",
            &format!("repos/spinyfin/mono/branches/{branch}"),
            "--jq",
            ".commit.sha",
        ],
        &format!("{sha}\n"),
    )
}

/// The fast-forward check: GitHub's head must be an ancestor of `@`.
fn ancestry_cmd(cwd: &std::path::Path, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        cwd.to_path_buf(),
        "jj",
        &[
            "log",
            "-r",
            &format!("present({HEAD_SHA}) & ancestors(@)"),
            "--no-graph",
            "-T",
            "commit_id",
        ],
        out,
    )
}

/// Build the standard remote-list response for a github-remote workspace.
pub(super) fn remote_list_github() -> &'static str {
    "origin\t/Users/bduff/dev/agents/repos/mono\ngithub\tgit@github.com:spinyfin/mono.git\n"
}

#[test]
fn pr_push_happy_path_advance() {
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        // remote list → github remote
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        // check PR is open
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        // @ is not empty
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "false",
        ),
        // fast-forward check against GitHub's current head
        github_head_cmd(&cwd, "boss/exec_abc", HEAD_SHA),
        ancestry_cmd(&cwd, "aabbcc\n"),
        // advance head-branch bookmark
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
        // push (no --allow-new)
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", "boss/exec_abc", "--remote", "github"],
            "",
        ),
        // verify: local sha
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
            "deadbeef\n",
        ),
        // verify: github sha
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "deadbeef\n",
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = run_with_dependencies(cli, None, &runner).expect("pr_push happy path");
    runner.assert_exhausted();
    assert_eq!(result.payload["action"], "pushed");
    assert_eq!(result.payload["number"], 42);
    assert!(result.payload["url"].as_str().unwrap().contains("/pull/42"));
}

#[test]
fn pr_push_noop_idempotency() {
    // @ is empty AND local branch matches GitHub head → no-op success.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        // @ is empty
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "true",
        ),
        // fetch github sha for head-branch
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "abc123\n",
        ),
        // the local head branch matches github
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let result = run_with_dependencies(cli, None, &runner).expect("pr_push noop");
    runner.assert_exhausted();
    assert_eq!(result.payload["action"], "noop");
    assert_eq!(result.payload["number"], 42);
}

#[test]
fn pr_push_empty_at_nothing_to_land() {
    // @ is empty AND local branch does NOT match GitHub head → error.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "true",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "github_sha\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
            "local_sha\n",
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("should fail — nothing to land");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("empty") && err.to_string().contains("nothing to land"),
        "error should mention empty and nothing to land: {err}"
    );
}

#[test]
fn pr_push_detached_refusal() {
    // @ is not a descendant of GitHub's head → refuse.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "false",
        ),
        // ancestor check returns empty → not a descendant
        github_head_cmd(&cwd, "boss/exec_abc", HEAD_SHA),
        ancestry_cmd(&cwd, ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let err = run_with_dependencies(cli, None, &runner).expect_err("should refuse detached @");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("not a descendant") || err.to_string().contains("descendant"),
        "error should mention descendant: {err}"
    );
}

#[test]
fn pr_push_stale_push_error() {
    // @ is non-empty, is a descendant, but jj git push fails (stale remote head).
    let cwd = std::env::current_dir().expect("cwd");
    let push_err = CubeError::CommandFailed {
        program: "jj".to_string(),
        args: vec![
            "git".to_string(),
            "push".to_string(),
            "-b".to_string(),
            "boss/exec_abc".to_string(),
            "--remote".to_string(),
            "github".to_string(),
        ],
        status: Some(1),
        stderr: "Error: Remote bookmark boss/exec_abc@github is ahead of local bookmark".to_string(),
    };
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "false",
        ),
        github_head_cmd(&cwd, "boss/exec_abc", HEAD_SHA),
        ancestry_cmd(&cwd, "aabbcc\n"),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
        // push fails
        ExpectedCommand {
            cwd: cwd.clone(),
            program: "jj".to_string(),
            args: ["git", "push", "-b", "boss/exec_abc", "--remote", "github"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            result: Err(push_err),
            creates_dir: None,
            duration: std::time::Duration::ZERO,
        },
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let err = run_with_dependencies(cli, None, &runner).expect_err("should surface push error");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("push") || err.to_string().contains("boss/exec_abc"),
        "error should mention push failure: {err}"
    );
}

#[test]
fn pr_push_merged_pr_hard_error() {
    // PR is MERGED → hard error, no push attempted.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"closed","merged":true}"#,
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("should hard-error on merged PR");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("MERGED") || err.to_string().contains("merged"),
        "error should mention MERGED: {err}"
    );
}

#[test]
fn pr_push_closed_pr_hard_error() {
    // PR is CLOSED → hard error.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"closed","merged":false}"#,
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "boss/exec_abc"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("should hard-error on closed PR");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("CLOSED") || err.to_string().contains("non-open"),
        "error should mention closed/non-open: {err}"
    );
}

#[test]
fn pr_push_force_with_lease_happy_path() {
    // --force-with-lease: lease valid (fetched sha == github sha) → force push succeeds.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        // @ is not empty
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "false",
        ),
        // lease check: jj's view of remote tracking bookmark
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc@github", "--no-graph", "-T", "commit_id"],
            "remote_sha\n",
        ),
        // lease check: GitHub's actual head — matches jj's view
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "remote_sha\n",
        ),
        // advance bookmarks
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
        // Export the branch ref the git push reads.
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "export"], ""),
        // force push via git
        ExpectedCommand::ok(
            cwd.clone(),
            "git",
            &["push", "--force-with-lease", "github", "boss/exec_abc"],
            "",
        ),
        // verify
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
            "new_sha\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "new_sha\n",
        ),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "push",
        "--pr",
        "42",
        "--branch",
        "boss/exec_abc",
        "--force-with-lease",
    ]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = run_with_dependencies(cli, None, &runner).expect("force-with-lease happy path");
    runner.assert_exhausted();
    assert_eq!(result.payload["action"], "pushed");
    assert_eq!(result.payload["number"], 42);
}

#[test]
fn pr_push_force_with_lease_concurrent_advance_refusal() {
    // --force-with-lease: GitHub has advanced beyond last fetch → refuse.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "false",
        ),
        // lease check: jj's view of remote
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc@github", "--no-graph", "-T", "commit_id"],
            "old_sha\n",
        ),
        // lease check: GitHub advanced concurrently
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "new_sha_from_concurrent_push\n",
        ),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "push",
        "--pr",
        "42",
        "--branch",
        "boss/exec_abc",
        "--force-with-lease",
    ]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let err = run_with_dependencies(cli, None, &runner).expect_err("should refuse concurrent advance");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("force-with-lease refused") || err.to_string().contains("advanced"),
        "error should mention lease refusal: {err}"
    );
}

#[test]
fn pr_push_infers_from_ancestry() {
    // No --pr / --branch: infer from jj ancestry.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        // Inference query: the nearest local branch bookmark in ancestry
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "log",
                "-n",
                "20",
                "-r",
                "ancestors(@) & bookmarks()",
                "--no-graph",
                "-T",
                r#"local_bookmarks.map(|b| b.name()).join("\n") ++ "\n""#,
            ],
            "boss-base/exec_abc\nboss/exec_abc\nmain\n",
        ),
        // GitHub says which PR the branch heads
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/pulls?head=spinyfin:boss/exec_abc&state=open",
            ],
            r#"[{"number":42,"html_url":"https://github.com/spinyfin/mono/pull/42"}]"#,
        ),
        // check PR is open
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/pulls/42"],
            r#"{"state":"open","merged":false,"head":{"repo":{"full_name":"spinyfin/mono"}}}"#,
        ),
        // @ is not empty
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "empty"],
            "false",
        ),
        // ancestor check
        github_head_cmd(&cwd, "boss/exec_abc", HEAD_SHA),
        ancestry_cmd(&cwd, "aabbcc\n"),
        // advance bookmarks
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "boss/exec_abc", "-r", "@"], ""),
        // push
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", "boss/exec_abc", "--remote", "github"],
            "",
        ),
        // verify
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "boss/exec_abc", "--no-graph", "-T", "commit_id"],
            "deadbeef\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_abc",
                "--jq",
                ".commit.sha",
            ],
            "deadbeef\n",
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "push"]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = run_with_dependencies(cli, None, &runner).expect("pr_push inferred from ancestry");
    runner.assert_exhausted();
    assert_eq!(result.payload["action"], "pushed");
    assert_eq!(result.payload["number"], 42);
}

#[test]
fn pr_push_guard_rejects_pr_bookmark_head_branch() {
    // If the resolved head-branch is a pr/* name (explicit --branch pr/42), refuse.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![ExpectedCommand::ok(
        cwd.clone(),
        "jj",
        &["git", "remote", "list"],
        remote_list_github(),
    )]);

    let cli = Cli::parse_from(["cube", "pr", "push", "--pr", "42", "--branch", "pr/42"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("should refuse pr/* branch");
    runner.assert_exhausted();
    assert!(
        err.to_string().contains("reserved") || err.to_string().contains("pr/42"),
        "error should mention reserved namespace: {err}"
    );
}

// --- pr push fork guard ---

#[test]
fn pr_push_refuses_fork_main_before_mutation() {
    for force in [false, true] {
        for explicit_branch in [false, true] {
            let cwd = std::env::current_dir().unwrap();
            let runner = FakeRunner::new(vec![
                ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
                ExpectedCommand::ok(
                    cwd.clone(),
                    "gh",
                    &["api", "repos/spinyfin/mono/pulls/42"],
                    r#"{"state":"open","head":{"ref":"main","repo":{"full_name":"contributor/mono"}}}"#,
                ),
            ]);
            let mut args = vec!["cube", "pr", "push", "--pr", "42"];
            if explicit_branch {
                args.extend(["--branch", "main"]);
            }
            if force {
                args.push("--force-with-lease");
            }
            let err = run_with_dependencies(Cli::parse_from(args), None, &runner).unwrap_err();
            assert!(matches!(err, CubeError::InvalidArgument(_)));
            assert!(err.to_string().contains("refusing to push a fork"));
            // The exact queue permits neither bookmark mutation nor push.
            runner.assert_exhausted();
        }
    }
}
