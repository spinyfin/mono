use std::path::PathBuf;

use super::support::{ExpectedCommand, FakeRunner};

use crate::app::errors::{CubeError, Result};
use crate::app::workspace_ops::{RebaseOpts, rebase_workspace_branch};

const REBASE_CWD: &str = "/ws";
const REBASE_REMOTE: &str = "github";
const REBASE_OWNER_REPO: &str = "spinyfin/mono";
const ANCESTRY_TMPL: &str = r#"bookmarks ++ " " ++ remote_bookmarks ++ "\n""#;
pub(super) const CONFLICT_TMPL: &str = r#"if(conflict, "CONFLICT", "CLEAN")"#;
const CONFLICT_COMMITS_TMPL: &str = r#"if(conflict, commit_id ++ "\n")"#;

fn rebase_cwd() -> PathBuf {
    PathBuf::from(REBASE_CWD)
}

/// A scripted command expectation whose result is an error — for probing
/// "revision doesn't exist" and push-failure paths.
pub(super) fn failing_cmd(cwd: PathBuf, program: &str, args: &[&str], stderr: &str) -> ExpectedCommand {
    let args_owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    ExpectedCommand {
        cwd,
        program: program.to_string(),
        args: args_owned.clone(),
        result: Err(CubeError::CommandFailed {
            program: program.to_string(),
            args: args_owned,
            status: Some(1),
            stderr: stderr.to_string(),
        }),
        creates_dir: None,
    }
}

fn rebase_opts(bookmark: Option<&str>, pr: Option<u64>, no_push: bool) -> RebaseOpts {
    RebaseOpts {
        explicit_bookmark: bookmark.map(str::to_string),
        explicit_pr: pr,
        no_push,
    }
}

/// The `jj git fetch` that every rebase begins with.
fn fetch_cmd() -> ExpectedCommand {
    ExpectedCommand::ok(rebase_cwd(), "jj", &["git", "fetch", "--remote", REBASE_REMOTE], "")
}

/// Existence probe for `<branch>@<remote>` → resolves a commit id.
fn remote_exists_cmd(branch: &str, sha: &str) -> ExpectedCommand {
    let remote_ref = format!("{branch}@{REBASE_REMOTE}");
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
        sha,
    )
}

/// Positioned probe: `<branch>@<remote> & ::@` → empty means mispositioned.
fn positioned_cmd(branch: &str, out: &str) -> ExpectedCommand {
    let revset = format!("{branch}@{REBASE_REMOTE} & ::@");
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", &revset, "--no-graph", "-T", "commit_id"],
        out,
    )
}

/// The track + set + rebase + conflict-check quartet shared by every
/// rebase that gets as far as actually rebasing. `conflicted_commit_ids`
/// is the set of commit ids the range conflict-check (`main@github..@`)
/// should report as conflicted — empty means the whole range rebased
/// clean.
fn set_track_rebase_check_cmds(branch: &str, conflicted_commit_ids: &[&str]) -> Vec<ExpectedCommand> {
    let remote_ref = format!("{branch}@{REBASE_REMOTE}");
    let conflict_out = if conflicted_commit_ids.is_empty() {
        String::new()
    } else {
        format!("{}\n", conflicted_commit_ids.join("\n"))
    };
    vec![
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["bookmark", "set", branch, "-r", &remote_ref, "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(rebase_cwd(), "jj", &["bookmark", "track", &remote_ref], ""),
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["rebase", "-b", branch, "-d", "main@github", "--ignore-immutable"],
            "rebased",
        ),
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["log", "-r", "main@github..@", "--no-graph", "-T", CONFLICT_COMMITS_TMPL],
            &conflict_out,
        ),
    ]
}

/// A `jj resolve --list -r <commit_id>` expectation for one conflicted
/// commit in the range — the per-commit enumeration `rebase_workspace_branch`
/// runs once per id the range conflict-check reports.
fn resolve_list_for_commit_cmd(commit_id: &str, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(rebase_cwd(), "jj", &["resolve", "--list", "-r", commit_id], out)
}

/// Push + verify, the clean-rebase tail.
fn push_and_verify_cmds(branch: &str) -> Vec<ExpectedCommand> {
    let api_path = format!("repos/{REBASE_OWNER_REPO}/branches/{branch}");
    vec![
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["git", "push", "-b", branch, "--remote", REBASE_REMOTE],
            "",
        ),
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
            "pushed1",
        ),
        ExpectedCommand::ok(
            rebase_cwd(),
            "gh",
            &["api", &api_path, "--jq", ".commit.sha"],
            "pushed1",
        ),
    ]
}

fn run_rebase(runner: &FakeRunner, opts: &RebaseOpts) -> Result<crate::app::errors::RunResult> {
    rebase_workspace_branch(
        runner,
        None,
        &rebase_cwd(),
        "main",
        REBASE_REMOTE,
        REBASE_OWNER_REPO,
        opts,
    )
}

#[test]
fn rebase_happy_path_positioned_clean_then_pushes() {
    let branch = "boss/exec_abc";
    let mut cmds = vec![
        fetch_cmd(),
        // ancestry fast path finds the boss bookmark
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["log", "-r", "ancestors(@, 5)", "--no-graph", "-T", ANCESTRY_TMPL],
            "boss/exec_abc boss/exec_abc@github\n",
        ),
        remote_exists_cmd(branch, "aaa111"),
        positioned_cmd(branch, "aaa111"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    cmds.extend(push_and_verify_cmds(branch));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(None, None, false)).expect("clean rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "clean");
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["pushed"], true);
    assert!(result.message.starts_with("REBASED_CLEAN"));
}

#[test]
fn rebase_not_positioned_errors_with_goto_hint() {
    // Without `cube workspace goto` pre-positioning, ancestry finds nothing
    // and rebase errors with a clear message pointing at the remedy.
    let cmds = vec![
        fetch_cmd(),
        // ancestry: @ on main, no boss bookmark in 5 ancestors
        ExpectedCommand::ok(
            rebase_cwd(),
            "jj",
            &["log", "-r", "ancestors(@, 5)", "--no-graph", "-T", ANCESTRY_TMPL],
            "main main@github\n",
        ),
    ];
    let runner = FakeRunner::new(cmds);
    let err = run_rebase(&runner, &rebase_opts(None, None, false)).expect_err("not positioned");
    runner.assert_exhausted();
    let msg = format!("{err}");
    assert!(msg.contains("boss/exec_*"), "names the expected pattern: {msg}");
    assert!(msg.contains("cube workspace goto"), "points at the remedy: {msg}");
}

#[test]
fn rebase_explicit_bookmark_skips_discovery_and_strips_remote_suffix() {
    // Multiple bookmarks may exist; --bookmark picks one with no discovery
    // queries, and a trailing @<remote> suffix is stripped.
    let branch = "boss/exec_xyz";
    let mut cmds = vec![fetch_cmd(), remote_exists_cmd(branch, "ccc")];
    cmds.push(positioned_cmd(branch, "ccc"));
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    cmds.extend(push_and_verify_cmds(branch));

    let runner = FakeRunner::new(cmds);
    // pass the @origin form; expect it stripped to the plain name
    let result = run_rebase(&runner, &rebase_opts(Some("boss/exec_xyz@github"), None, false)).expect("explicit rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["pushed"], true);
}

#[test]
fn rebase_pr_arg_resolves_head_branch_from_github() {
    let branch = "boss/exec_pr7";
    let mut cmds = vec![
        fetch_cmd(),
        ExpectedCommand::ok(
            rebase_cwd(),
            "gh",
            &["pr", "view", "7", "-R", REBASE_OWNER_REPO, "--json", "headRefName"],
            r#"{"headRefName":"boss/exec_pr7"}"#,
        ),
        remote_exists_cmd(branch, "e1"),
        positioned_cmd(branch, "e1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    cmds.extend(push_and_verify_cmds(branch));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(None, Some(7), false)).expect("pr rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["pushed"], true);
}

#[test]
fn rebase_conflicts_skip_push_and_name_the_bookmark() {
    let branch = "boss/exec_c";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "f1"),
        positioned_cmd(branch, "f1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["tip1"]));
    cmds.push(resolve_list_for_commit_cmd("tip1", "src/foo.rs\nsrc/bar.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("conflict rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["pushed"], false);
    assert_eq!(result.payload["conflicted_commit_count"], 1);
    assert_eq!(result.payload["conflicted_files"][0], "src/foo.rs");
    assert!(result.message.starts_with("REBASED_WITH_CONFLICTS"));
    assert!(
        result.message.contains("jj git push -b boss/exec_c"),
        "conflict message names the exact push command: {}",
        result.message
    );
}

#[test]
fn rebase_conflicts_span_ancestor_commit_beyond_the_working_copy_tip() {
    // Regression coverage: a rebase can leave an ANCESTOR commit
    // individually conflicted even when the working-copy tip's own
    // merged tree looks clean for that path (a later commit's edits can
    // make the tip's `jj resolve --list` miss it entirely). The range
    // check (`main@github..@`) must catch both the tip's own conflict
    // and the lower commit's, and the aggregated `conflicted_files` must
    // include files from both — not just the tip's.
    let branch = "boss/exec_multi";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "m1"),
        positioned_cmd(branch, "m1"),
    ];
    // Two conflicted commits in the range: the tip and one ancestor.
    cmds.extend(set_track_rebase_check_cmds(branch, &["tip9", "ancestor1"]));
    cmds.push(resolve_list_for_commit_cmd("tip9", "BUILD.bazel\n"));
    cmds.push(resolve_list_for_commit_cmd("ancestor1", "MODULE.bazel.lock\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("multi-commit conflict rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["pushed"], false);
    assert_eq!(result.payload["conflicted_commit_count"], 2);
    let files: Vec<&str> = result.payload["conflicted_files"]
        .as_array()
        .expect("conflicted_files is an array")
        .iter()
        .map(|v| v.as_str().expect("file entry is a string"))
        .collect();
    assert_eq!(
        files,
        vec!["BUILD.bazel", "MODULE.bazel.lock"],
        "must include the ancestor commit's conflicted file, not just the tip's: {files:?}"
    );
}

#[test]
fn rebase_conflicts_dedupe_a_file_conflicted_on_more_than_one_commit() {
    // Cascading conflicts: the same path can show as conflicted on more
    // than one commit in the range (a descendant inheriting an
    // unresolved region from its parent). The aggregated list must not
    // report the same file twice.
    let branch = "boss/exec_dupe";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "d1"),
        positioned_cmd(branch, "d1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["tip5", "ancestor5"]));
    cmds.push(resolve_list_for_commit_cmd("tip5", "src/shared.rs\n"));
    cmds.push(resolve_list_for_commit_cmd("ancestor5", "src/shared.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("dedupe conflict rebase");
    runner.assert_exhausted();
    let files = result.payload["conflicted_files"].as_array().expect("array");
    assert_eq!(
        files.len(),
        1,
        "must dedupe the file shared by both conflicted commits: {files:?}"
    );
    assert_eq!(files[0], "src/shared.rs");
}

#[test]
fn rebase_no_push_flag_skips_advance_and_push() {
    let branch = "boss/exec_np";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "g1"),
        positioned_cmd(branch, "g1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    // no push/verify commands expected

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, true)).expect("no-push rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "clean");
    assert_eq!(result.payload["pushed"], false);
    assert!(result.message.contains("Push skipped"), "message: {}", result.message);
}

#[test]
fn rebase_branch_missing_on_remote_errors_clearly() {
    let branch = "boss/exec_missing";
    let remote_ref = format!("{branch}@{REBASE_REMOTE}");
    let cmds = vec![
        fetch_cmd(),
        failing_cmd(
            rebase_cwd(),
            "jj",
            &["log", "-r", &remote_ref, "--no-graph", "-T", "commit_id"],
            "Error: Revision \"boss/exec_missing@github\" doesn't exist",
        ),
    ];
    let runner = FakeRunner::new(cmds);
    let err = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect_err("missing branch");
    runner.assert_exhausted();
    let msg = format!("{err}");
    assert!(msg.contains("was not found on remote"), "{msg}");
    assert!(msg.contains(branch), "{msg}");
}

#[test]
fn rebase_push_failure_surfaces_clear_error_without_force_flags() {
    let branch = "boss/exec_pf";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "h1"),
        positioned_cmd(branch, "h1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    cmds.push(failing_cmd(
        rebase_cwd(),
        "jj",
        &["git", "push", "-b", branch, "--remote", REBASE_REMOTE],
        "Error: remote bookmark changed",
    ));

    let runner = FakeRunner::new(cmds);
    let err = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect_err("push failed");
    runner.assert_exhausted();
    let msg = format!("{err}");
    assert!(msg.contains("pushing it to"), "{msg}");
    assert!(msg.contains("re-run"), "guides re-running, not forcing: {msg}");
    assert!(!msg.contains("--force"), "must not suggest a force flag: {msg}");
}

#[test]
fn rebase_mispositioned_at_self_heals_with_jj_new_before_rebase() {
    // When `@` is on main (not on/after the boss branch), the self-heal path
    // must run `jj new <branch>@<remote>` before the bookmark-set/rebase
    // quartet so the working copy lands on the branch head.
    let branch = "boss/exec_heal";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "heal1"),
        // positioned probe returns empty → @ is not on/after the boss head
        positioned_cmd(branch, ""),
        // self-heal: create an editable child of the remote boss head
        ExpectedCommand::ok(rebase_cwd(), "jj", &["new", &format!("{branch}@{REBASE_REMOTE}")], ""),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    cmds.extend(push_and_verify_cmds(branch));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("self-heal rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "clean");
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["pushed"], true);
    assert!(result.message.starts_with("REBASED_CLEAN"));
}

// ───────────────────────── workspace push ─────────────────────────
//
// `workspace_push` is the testable unit for `cube workspace push`. Like
// `pr_push`, it reads `std::env::current_dir()` directly rather than
// taking an explicit `cwd` param, so these tests capture the ambient
// test-process cwd (matching the `pr_push` test convention) instead of
// a synthetic path.
