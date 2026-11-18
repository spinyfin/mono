use std::path::PathBuf;

use super::rebase_tests::failing_cmd;
use super::support::{ExpectedCommand, FakeRunner};

use crate::app::errors::{CubeError, Result};
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
        &[
            "log",
            "-r",
            &format!("present({remote_ref})"),
            "--no-graph",
            "-T",
            "commit_id",
        ],
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

fn goto_jj(args: &[&str], out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(goto_cwd(), "jj", args, out)
}

/// What `goto --pr` runs after GitHub's PR JSON: confirm `head` is in the
/// store and check whether the working copy is already on it (`positioned`
/// is that probe's output; empty means `jj new` follows). The head branch
/// bookmark is set only after the working copy has moved.
fn goto_pr_head_cmds(head: &str, positioned: &str) -> Vec<ExpectedCommand> {
    let mut cmds = vec![
        goto_jj(
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present({head})"),
                "-T",
                r#"commit_id ++ "\n""#,
            ],
            head,
        ),
        goto_pr_positioned_cmd(head, positioned),
    ];
    if positioned.is_empty() {
        cmds.push(goto_jj(&["new", head], ""));
    }
    cmds
}

fn goto_pr_json(branch: &str, head: &str) -> String {
    format!(
        r#"{{"head":{{"repo":{{"full_name":"spinyfin/mono"}},"ref":"{branch}","sha":"{head}"}},"state":"open","merged":false}}"#
    )
}

fn goto_pr_positioned_cmd(head: &str, out: &str) -> ExpectedCommand {
    goto_jj(
        &["log", "-r", &format!("{head} & ::@"), "--no-graph", "-T", "commit_id"],
        out,
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
        None,
    )
}

fn run_goto_revision(runner: &FakeRunner, revision: &str) -> Result<crate::app::errors::RunResult> {
    workspace_goto(
        None,
        runner,
        Some(GOTO_CWD.to_string()),
        None,
        None,
        Some(revision.to_string()),
    )
}

fn goto_revision_positioned_cmd(sha: &str, found_sha: &str) -> ExpectedCommand {
    let revset = format!("parents(@) & {sha}");
    ExpectedCommand::ok(
        goto_cwd(),
        "jj",
        &["log", "-r", &revset, "--no-graph", "-T", "commit_id"],
        found_sha,
    )
}

fn goto_revision_new_cmd(sha: &str) -> ExpectedCommand {
    ExpectedCommand::ok(goto_cwd(), "jj", &["new", sha], "")
}

#[test]
fn goto_bookmark_not_yet_positioned_creates_new_commit() {
    let branch = "boss/exec_goto1";
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        goto_exists_cmd(branch, "abc1"),
        goto_positioned_cmd(branch, ""), // not yet positioned
        goto_new_cmd(branch),
        goto_set_bookmark_cmd(branch),
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
        goto_positioned_cmd(branch, "def2"), // already positioned
        goto_set_bookmark_cmd(branch),
        // no jj new expected
    ];
    let runner = FakeRunner::new(cmds);
    let result = run_goto(&runner, Some(branch), None).expect("idempotent goto");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["already_positioned"], true);
}

#[test]
/// `goto --pr` positions on GitHub's head and tracks the head branch. The
/// scripted runner is a strict FIFO, so any `pr/<n>` import, set or export
/// would panic as unexpected.
fn goto_pr_resolves_branch_and_creates_no_pr_bookmark() {
    let branch = "boss/exec_gotopr";
    let mut cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        ExpectedCommand::ok(
            goto_cwd(),
            "gh",
            &["api", &format!("repos/{GOTO_OWNER_REPO}/pulls/42")],
            &goto_pr_json(branch, "e42e42e42e42e42e42e42e42e42e42e42e42e42e"),
        ),
    ];
    cmds.extend(goto_pr_head_cmds("e42e42e42e42e42e42e42e42e42e42e42e42e42e", ""));
    cmds.push(goto_jj(
        &[
            "bookmark",
            "set",
            branch,
            "-r",
            "e42e42e42e42e42e42e42e42e42e42e42e42e42e",
            "--allow-backwards",
        ],
        "",
    ));
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
        ExpectedCommand::ok(
            goto_cwd(),
            "jj",
            &[
                "log",
                "-r",
                &format!("present({remote_ref})"),
                "--no-graph",
                "-T",
                "commit_id",
            ],
            "",
        ),
    ];
    let runner = FakeRunner::new(cmds);
    let err = run_goto(&runner, Some(branch), None).expect_err("missing bookmark");
    runner.assert_exhausted();
    assert!(matches!(err, CubeError::PositioningRefused(_)));
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

#[test]
fn goto_revision_not_yet_positioned_creates_new_commit() {
    // Regression coverage: the probe must test `parents(@) & {sha}`, not
    // ancestry (`{sha} & ::@`). A merge SHA on the default branch is an
    // ancestor of `@` in almost any workspace whose `@` descends from a
    // `main` that already contains the merge — but `@` is not actually
    // positioned there. `parents(@) & {sha}` must come back empty in that
    // case so `jj new` still runs, even though the old ancestor revset
    // would have reported `already_positioned: true` here. The assertion
    // below on the probe's revset argument (via `goto_revision_positioned_cmd`
    // building `parents(@) & {sha}`, which `FakeRunner` requires an exact
    // match against) is what actually pins this down — the ancestor revset
    // cannot be reintroduced without this test failing.
    let sha = "merge-sha-1";
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        goto_revision_positioned_cmd(sha, ""), // not yet positioned
        goto_revision_new_cmd(sha),
    ];
    let runner = FakeRunner::new(cmds);
    let result = run_goto_revision(&runner, sha).expect("revision goto succeeds");
    runner.assert_exhausted();
    assert_eq!(result.payload["revision"], sha);
    assert_eq!(result.payload["already_positioned"], false);
}

#[test]
fn goto_revision_already_positioned_skips_new() {
    let sha = "merge-sha-2";
    let cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        goto_revision_positioned_cmd(sha, sha), // already positioned
    ];
    let runner = FakeRunner::new(cmds);
    let result = run_goto_revision(&runner, sha).expect("idempotent revision goto");
    runner.assert_exhausted();
    assert_eq!(result.payload["already_positioned"], true);
}

#[test]
fn goto_closed_pr_is_a_structured_refusal_without_mutating_bookmarks() {
    for state in ["closed", "merged"] {
        let info = if state == "merged" {
            r#"{"state":"closed","merged":true,"head":{"ref":"feature"}}"#
        } else {
            r#"{"state":"closed","merged":false,"head":{"ref":"feature"}}"#
        };
        let runner = FakeRunner::new(vec![
            goto_remote_list_cmd(),
            goto_fetch_cmd(),
            ExpectedCommand::ok(
                goto_cwd(),
                "gh",
                &["api", &format!("repos/{GOTO_OWNER_REPO}/pulls/42")],
                info,
            ),
        ]);
        let error = run_goto(&runner, None, Some(42)).unwrap_err();
        runner.assert_exhausted();
        assert!(matches!(error, CubeError::PositioningRefused(_)));
        assert_eq!(error.json_error()["error_kind"], "workspace_positioning_refused");
        assert_eq!(error.json_error()["retryable"], false);
        assert!(error.to_string().contains("cannot position"));
    }
}

#[test]
fn goto_fetch_failure_does_not_claim_retryability() {
    let runner = FakeRunner::new(vec![
        goto_remote_list_cmd(),
        failing_cmd(
            goto_cwd(),
            "jj",
            &["git", "fetch", "--remote", GOTO_REMOTE],
            "network unavailable",
        ),
    ]);
    let error = run_goto(&runner, None, Some(42)).unwrap_err();
    assert!(matches!(error, CubeError::CommandFailed { .. }));
    assert!(error.json_error().get("retryable").is_none());
    assert!(error.json_error().get("error_kind").is_none());
}

#[test]
fn unclassified_errors_do_not_claim_to_be_retryable() {
    let error = CubeError::InvalidArgument("bad flag".to_string());
    assert!(error.json_error().get("retryable").is_none());
    assert!(error.json_error().get("error_kind").is_none());
    assert!(
        CubeError::RepoNotFound("mono".into())
            .json_error()
            .get("error_kind")
            .is_none()
    );
    assert_eq!(
        CubeError::PositioningRefused("x".into()).json_error()["retryable"],
        false
    );
    assert_eq!(
        CubeError::PositioningRefused("x".into()).json_error()["error_kind"],
        "workspace_positioning_refused"
    );
    assert_eq!(CubeError::MaintenanceBusy.json_error()["retryable"], true);
}
