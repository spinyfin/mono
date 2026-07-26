use std::path::PathBuf;

use super::rebase_tests::failing_cmd;
use super::support::{ExpectedCommand, FakeRunner};

use crate::app::errors::Result;
use crate::app::workspace_ops::workspace_goto;

pub(super) const GOTO_CWD: &str = "/goto-ws";
const GOTO_REMOTE: &str = "github";
const GOTO_OWNER_REPO: &str = "spinyfin/mono";

fn goto_cwd() -> PathBuf {
    PathBuf::from(GOTO_CWD)
}

fn goto_remote_list_cmd() -> ExpectedCommand {
    ExpectedCommand::ok(
        goto_cwd(),
        "jj",
        &["git", "remote", "list"],
        &format!("origin\t/local/mirror\n{GOTO_REMOTE}\thttps://github.com/{GOTO_OWNER_REPO}\n"),
    )
}

fn goto_fetch_cmd() -> ExpectedCommand {
    ExpectedCommand::ok(goto_cwd(), "jj", &["git", "fetch", "--remote", GOTO_REMOTE], "")
}

fn goto_exists_cmd(branch: &str, sha: &str) -> ExpectedCommand {
    let remote_ref = format!("{branch}@{GOTO_REMOTE}");
    ExpectedCommand::ok(
        goto_cwd(),
        "jj",
        &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
        sha,
    )
}

fn goto_set_bookmark_cmd(branch: &str) -> ExpectedCommand {
    let remote_ref = format!("{branch}@{GOTO_REMOTE}");
    ExpectedCommand::ok(
        goto_cwd(),
        "jj",
        &["bookmark", "set", branch, "-r", &remote_ref, "--allow-backwards"],
        "",
    )
}

fn goto_set_pr_bookmark_cmd(pr: u64, branch: &str) -> ExpectedCommand {
    let remote_ref = format!("{branch}@{GOTO_REMOTE}");
    let pr_bm = format!("pr/{pr}");
    ExpectedCommand::ok(
        goto_cwd(),
        "jj",
        &["bookmark", "set", &pr_bm, "-r", &remote_ref, "--allow-backwards"],
        "",
    )
}

fn goto_positioned_cmd(branch: &str, sha: &str) -> ExpectedCommand {
    let revset = format!("{branch}@{GOTO_REMOTE} & ::@");
    ExpectedCommand::ok(
        goto_cwd(),
        "jj",
        &["log", "-r", &revset, "--no-graph", "-T", "commit_id"],
        sha,
    )
}

fn goto_new_cmd(branch: &str) -> ExpectedCommand {
    let remote_ref = format!("{branch}@{GOTO_REMOTE}");
    ExpectedCommand::ok(goto_cwd(), "jj", &["new", &remote_ref], "")
}

fn run_goto(runner: &FakeRunner, bookmark: Option<&str>, pr: Option<u64>) -> Result<crate::app::errors::RunResult> {
    workspace_goto(
        None,
        runner,
        Some(GOTO_CWD.to_string()),
        bookmark.map(str::to_string),
        pr,
    )
}

#[test]
fn goto_bookmark_not_yet_positioned_creates_new_commit() {
    let branch = "boss/exec_goto1";
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        goto_exists_cmd(branch, "abc1"),
        goto_set_bookmark_cmd(branch),
        goto_positioned_cmd(branch, ""), // not yet positioned
        goto_new_cmd(branch),
    ];
    let runner = FakeRunner::new(cmds);
    let result = run_goto(&runner, Some(branch), None).expect("goto succeeds");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["already_positioned"], false);
}

#[test]
fn goto_bookmark_already_positioned_skips_jj_new() {
    let branch = "boss/exec_goto2";
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        goto_exists_cmd(branch, "def2"),
        goto_set_bookmark_cmd(branch),
        goto_positioned_cmd(branch, "def2"), // already positioned
                                             // no jj new expected
    ];
    let runner = FakeRunner::new(cmds);
    let result = run_goto(&runner, Some(branch), None).expect("idempotent goto");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["already_positioned"], true);
}

#[test]
fn goto_pr_resolves_branch_and_sets_pr_bookmark() {
    let branch = "boss/exec_gotopr";
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        ExpectedCommand::ok(
            goto_cwd(),
            "gh",
            &["pr", "view", "42", "-R", GOTO_OWNER_REPO, "--json", "headRefName,state"],
            &format!(r#"{{"headRefName":"{branch}","state":"OPEN"}}"#),
        ),
        goto_exists_cmd(branch, "e42"),
        goto_set_bookmark_cmd(branch),
        goto_set_pr_bookmark_cmd(42, branch),
        goto_positioned_cmd(branch, ""),
        goto_new_cmd(branch),
    ];
    let runner = FakeRunner::new(cmds);
    let result = run_goto(&runner, None, Some(42)).expect("pr goto");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["already_positioned"], false);
}

#[test]
fn goto_missing_bookmark_errors_clearly() {
    let branch = "boss/exec_missing";
    let remote_ref = format!("{branch}@{GOTO_REMOTE}");
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        failing_cmd(
            goto_cwd(),
            "jj",
            &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
            &format!("Error: Revision \"{remote_ref}\" doesn't exist"),
        ),
    ];
    let runner = FakeRunner::new(cmds);
    let err = run_goto(&runner, Some(branch), None).expect_err("missing bookmark");
    runner.assert_exhausted();
    let msg = format!("{err}");
    assert!(msg.contains(branch), "names the branch: {msg}");
    assert!(msg.contains("was not found on remote"), "{msg}");
}

#[test]
fn goto_requires_bookmark_or_pr() {
    let cmds = vec![goto_remote_list_cmd(), goto_fetch_cmd()];
    let runner = FakeRunner::new(cmds);
    let err = run_goto(&runner, None, None).expect_err("requires arg");
    let msg = format!("{err}");
    assert!(msg.contains("--bookmark") || msg.contains("--pr"), "{msg}");
}
