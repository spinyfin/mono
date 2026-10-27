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

fn goto_jj(args: &[&str], out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(goto_cwd(), "jj", args, out)
}

fn goto_pr_bookmark_commits_cmd(pr: u64, out: &str) -> ExpectedCommand {
    let revset = format!("bookmarks(exact:\"pr/{pr}\")");
    goto_jj(&["log", "--no-graph", "-r", &revset, "-T", r#"commit_id ++ "\n""#], out)
}

/// The commands `point_pr_bookmark` issues around the `pr/<n>` set, up to and
/// including the set itself: import any stale git ref, resolve the head, read
/// what `pr/<n>` currently resolves to (`before`).
fn goto_pr_bookmark_prelude(pr: u64, head: &str, before: &str) -> Vec<ExpectedCommand> {
    vec![
        goto_jj(&["git", "import"], ""),
        goto_jj(&["log", "--no-graph", "-r", head, "-T", r#"commit_id ++ "\n""#], head),
        goto_pr_bookmark_commits_cmd(pr, before),
    ]
}

fn goto_pr_bookmark_set_cmds(pr: u64, head: &str) -> Vec<ExpectedCommand> {
    let pr_bm = format!("pr/{pr}");
    vec![
        goto_jj(&["bookmark", "set", &pr_bm, "-r", head, "--allow-backwards"], ""),
        goto_jj(&["git", "export"], ""),
        goto_pr_bookmark_commits_cmd(pr, head),
        goto_jj(
            &[
                "log",
                "--no-graph",
                "-r",
                &format!("present(pr/{pr}@git)"),
                "-T",
                r#"commit_id ++ "\n""#,
            ],
            head,
        ),
    ]
}

/// Whole healthy-`pr/<n>` sequence: nothing there before, exactly the head after.
fn goto_set_pr_bookmark_cmds(pr: u64, head: &str) -> Vec<ExpectedCommand> {
    let mut cmds = goto_pr_bookmark_prelude(pr, head, "");
    cmds.extend(goto_pr_bookmark_set_cmds(pr, head));
    cmds
}

/// What `goto --pr` runs after GitHub's PR JSON: confirm `head` is in the
/// store, then point the local branch bookmark at it.
fn goto_pr_head_cmds(branch: &str, head: &str) -> Vec<ExpectedCommand> {
    vec![
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
        goto_jj(&["bookmark", "set", branch, "-r", head, "--allow-backwards"], ""),
    ]
}

fn goto_pr_json(branch: &str, head: &str) -> String {
    format!(r#"{{"head":{{"ref":"{branch}","sha":"{head}"}},"state":"open","merged":false}}"#)
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
    let mut cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        ExpectedCommand::ok(
            goto_cwd(),
            "gh",
            &["api", &format!("repos/{GOTO_OWNER_REPO}/pulls/42")],
            &goto_pr_json(branch, "e42"),
        ),
    ];
    cmds.extend(goto_pr_head_cmds(branch, "e42"));
    cmds.extend(goto_set_pr_bookmark_cmds(42, "e42"));
    cmds.push(goto_pr_positioned_cmd("e42", ""));
    cmds.push(goto_jj(&["new", "e42"], ""));
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

/// A conflicted `pr/<n>` whose other side is an ancestor of the head (nothing
/// unique) is repaired to the head: no forget, no retry.
#[test]
fn goto_pr_repairs_conflicted_bookmark_when_other_side_is_an_ancestor() {
    let branch = "boss/exec_gotopr";
    let mut cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        ExpectedCommand::ok(
            goto_cwd(),
            "gh",
            &["api", &format!("repos/{GOTO_OWNER_REPO}/pulls/42")],
            &goto_pr_json(branch, "e42"),
        ),
    ];
    cmds.extend(goto_pr_head_cmds(branch, "e42"));
    cmds.extend(goto_pr_bookmark_prelude(42, "e42", "e42\nold1\n"));
    cmds.push(goto_jj(
        &[
            "log",
            "--no-graph",
            "-n",
            "101",
            "-r",
            "(::old1) ~ (::e42) ~ ::(present(main@origin) | present(master@origin) | present(main@github) | present(master@github))",
            "-T",
            r#"change_id ++ "\t" ++ commit_id ++ "\n""#,
        ],
        "",
    ));
    cmds.extend(goto_pr_bookmark_set_cmds(42, "e42"));
    cmds.push(goto_pr_positioned_cmd("e42", "e42"));
    let runner = FakeRunner::new(cmds);
    run_goto(&runner, None, Some(42)).expect("conflicted pr bookmark repaired");
    runner.assert_exhausted();
}

/// A conflicted `pr/<n>` whose other side carries a commit the head has no
/// rewrite of fails loudly, naming both commits, and writes nothing.
#[test]
fn goto_pr_refuses_conflicted_bookmark_with_unique_content() {
    let branch = "boss/exec_gotopr";
    let mut cmds = vec![
        goto_remote_list_cmd(),
        goto_fetch_cmd(),
        ExpectedCommand::ok(
            goto_cwd(),
            "gh",
            &["api", &format!("repos/{GOTO_OWNER_REPO}/pulls/42")],
            &goto_pr_json(branch, "e42"),
        ),
    ];
    cmds.extend(goto_pr_head_cmds(branch, "e42"));
    cmds.extend(goto_pr_bookmark_prelude(42, "e42", "e42\nuniq9\n"));
    cmds.push(goto_jj(
        &[
            "log",
            "--no-graph",
            "-n",
            "101",
            "-r",
            "(::uniq9) ~ (::e42) ~ ::(present(main@origin) | present(master@origin) | present(main@github) | present(master@github))",
            "-T",
            r#"change_id ++ "\t" ++ commit_id ++ "\n""#,
        ],
        "chg1\tuniq9\n",
    ));
    cmds.push(goto_jj(
        &[
            "log",
            "--no-graph",
            "-r",
            "present(chg1) & (::e42)",
            "-T",
            r#"commit_id ++ "\n""#,
        ],
        "",
    ));
    let runner = FakeRunner::new(cmds);
    let err = run_goto(&runner, None, Some(42)).expect_err("unique content must not be discarded");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(
        msg.contains("e42") && msg.contains("uniq9"),
        "names both commits: {msg}"
    );
    assert!(msg.contains("will not pick a side"), "{msg}");
}
