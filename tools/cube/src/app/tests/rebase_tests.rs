use std::path::PathBuf;

use super::support::{ExpectedCommand, FakeRunner};

use crate::app::errors::{CubeError, Result};
use crate::app::workspace_ops::{RebaseOpts, rebase_workspace_branch};

const REBASE_CWD: &str = "/ws";
const REBASE_REMOTE: &str = "github";
const REBASE_OWNER_REPO: &str = "spinyfin/mono";
const ANCESTRY_TMPL: &str = r#"bookmarks ++ " " ++ remote_bookmarks ++ "\n""#;
/// The one jj template that answers "is this commit conflicted?" —
/// shared with `push_tests` so both call sites stay in step.
pub(super) const CONFLICT_COMMITS_TMPL: &str = r#"if(conflict, commit_id ++ "\n")"#;

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
        duration: std::time::Duration::ZERO,
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

/// The first probe `try_linearize_replay_conflicts` runs once the range check
/// reports any conflict: is the branch head — the commit that would actually
/// be pushed — conflicted on its own? A non-empty `out` means yes, which is a
/// genuine content conflict and declines the collapse immediately.
fn head_conflict_probe_cmd(branch: &str, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", branch, "--no-graph", "-T", CONFLICT_COMMITS_TMPL],
        out,
    )
}

/// `jj log -r <branch> -T commit_id` — the pre-collapse head snapshot the
/// tree-equality gate later diffs against.
fn head_commit_id_cmd(branch: &str, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
        out,
    )
}

/// Conflicted-commit enumeration over an arbitrary revset — used by the
/// collapse loop over the push range and by its final re-check.
fn conflicts_in_revset_cmd(revset: &str, conflicted_commit_ids: &[&str]) -> ExpectedCommand {
    let out = if conflicted_commit_ids.is_empty() {
        String::new()
    } else {
        format!("{}\n", conflicted_commit_ids.join("\n"))
    };
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", revset, "--no-graph", "-T", CONFLICT_COMMITS_TMPL],
        &out,
    )
}

/// The operation-id capture that follows every rewrite the collapse issues,
/// so a later decline can undo exactly those operations — and nothing a
/// concurrent sibling workspace landed in the shared operation log.
fn op_id_cmd(out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["op", "log", "-n", "1", "--no-graph", "-T", "id"],
        out,
    )
}

/// The rollback a declining guard owes once at least one squash has landed:
/// one targeted `jj op undo` per operation this attempt issued. Never
/// `jj op restore`, which would reset the whole shared repo.
fn op_undo_cmd(op: &str) -> ExpectedCommand {
    ExpectedCommand::ok(rebase_cwd(), "jj", &["op", "undo", op], "")
}

/// `jj log -r <rev> -T description` — the message capture that precedes each
/// squash (and the head read that precedes the message merge).
fn description_cmd(revision: &str, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", revision, "--no-graph", "-T", "description"],
        out,
    )
}

/// The `jj describe` that restores the collapsed commits' messages onto the
/// surviving branch head.
fn describe_cmd(revision: &str, message: &str) -> ExpectedCommand {
    ExpectedCommand::ok(rebase_cwd(), "jj", &["describe", "-r", revision, "-m", message], "")
}

/// The stacked-PR guard probe: does any bookmark point at this commit?
fn bookmarks_at_cmd(commit_id: &str, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["log", "-r", commit_id, "--no-graph", "-T", "bookmarks"],
        out,
    )
}

/// `children(<commit>)` enumeration — the collapse needs exactly one.
fn children_of_cmd(commit_id: &str, child_ids: &[&str]) -> ExpectedCommand {
    let out = if child_ids.is_empty() {
        String::new()
    } else {
        format!("{}\n", child_ids.join("\n"))
    };
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &[
            "log",
            "-r",
            &format!("children({commit_id})"),
            "--no-graph",
            "-T",
            r#"commit_id ++ "\n""#,
        ],
        &out,
    )
}

/// The squash that collapses one replay-only conflicted commit forward.
fn squash_into_cmd(from: &str, into: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["squash", "--from", from, "--into", into, "--use-destination-message"],
        "",
    )
}

/// The tree-equality gate: `jj diff --from <pre-collapse head> --to <branch>
/// --summary`. Empty output proves the collapse changed history shape only.
fn tree_delta_cmd(head_before: &str, branch: &str, out: &str) -> ExpectedCommand {
    ExpectedCommand::ok(
        rebase_cwd(),
        "jj",
        &["diff", "--from", head_before, "--to", branch, "--summary"],
        out,
    )
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
            &["api", &format!("repos/{REBASE_OWNER_REPO}/pulls/7")],
            r#"{"head":{"ref":"boss/exec_pr7"}}"#,
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
    // The head itself is conflicted — a real content conflict, so the
    // replay-only collapse declines up front and this stays a hand-off.
    cmds.push(head_conflict_probe_cmd(branch, "tip1\n"));
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
    // The tip is among the conflicted commits, so this is genuine residue,
    // not a replay-only artifact: the collapse declines and both commits'
    // files must still be reported.
    cmds.push(head_conflict_probe_cmd(branch, "tip9\n"));
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
    cmds.push(head_conflict_probe_cmd(branch, "tip5\n"));
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

// ─────────────── replay-only conflict linearization ───────────────
//
// A rebase replays each commit individually, so an ancestor can conflict
// even when the branch head's rebased tree is already correct — `git
// merge-tree` of the same base/head pair reports the merge clean. jj keeps
// the flag on the lower commit and `jj git push` then refuses the whole
// range, so the rebase is finished but unpushable and every such PR was
// escalated to an agent for a conflict that does not exist at the head.
// These cover collapsing that forward, and each guard that must decline.

/// The message-merge `jj describe` the collapse issues once it verifies.
fn merged_description(head: &str, collapsed: &[&str]) -> String {
    let mut combined = head.trim_end().to_owned();
    if !combined.is_empty() {
        combined.push_str("\n\n");
    }
    combined.push_str("Collapsed replay-only conflicted commits (content unchanged; their messages follow):\n");
    for description in collapsed {
        combined.push('\n');
        combined.push_str(description.trim_end());
        combined.push('\n');
    }
    combined
}

/// The probe/pre-flight prologue every collapse attempt runs before it
/// touches anything: is the head clean, what is its commit id, what is
/// conflicted in the push range, and is each of those safe to collapse
/// (no bookmark, exactly one child)?
///
/// The guards being *pre-flighted over the whole set* is the point: a
/// stacked-PR boundary or a fork anywhere in the range is caught while the
/// workspace is still pristine, so those declines can never owe a rollback.
fn preflight_cmds(branch: &str, head_before: &str, targets: &[(&str, &str)]) -> Vec<ExpectedCommand> {
    let push_range = format!("main@{REBASE_REMOTE}..{branch}");
    let ids: Vec<&str> = targets.iter().map(|(id, _)| *id).collect();
    let mut cmds = vec![
        head_conflict_probe_cmd(branch, ""),
        head_commit_id_cmd(branch, &format!("{head_before}\n")),
        conflicts_in_revset_cmd(&push_range, &ids),
    ];
    for (target, child) in targets {
        cmds.push(bookmarks_at_cmd(target, ""));
        cmds.push(children_of_cmd(target, &[child]));
    }
    cmds
}

/// One collapse round for an already-vetted target: capture the message the
/// squash would drop, squash, and record the operation id so a later decline
/// can undo exactly this operation and nothing else.
fn collapse_round_cmds(
    push_range: &str,
    target: &str,
    child: &str,
    description: &str,
    op: &str,
) -> Vec<ExpectedCommand> {
    vec![
        conflicts_in_revset_cmd(push_range, &[target]),
        description_cmd(target, &format!("{description}\n")),
        squash_into_cmd(target, child),
        op_id_cmd(&format!("{op}\n")),
    ]
}

/// The full happy path: one conflicted ancestor under a clean head is
/// collapsed into its child, the range re-check comes back clean, the tree
/// is proven unchanged, the collapsed commit's message is merged into the
/// surviving head, and the normal clean push tail runs.
#[test]
fn rebase_collapses_a_replay_only_conflicted_ancestor_and_pushes() {
    let branch = "boss/exec_replay";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "r1"),
        positioned_cmd(branch, "r1"),
    ];
    // The range check sees one conflicted commit...
    cmds.extend(set_track_rebase_check_cmds(branch, &["ancestor7"]));
    // ...but the head that would be pushed is clean, so it is replay-only.
    cmds.extend(preflight_cmds(branch, "headbefore", &[("ancestor7", "child7")]));
    cmds.extend(collapse_round_cmds(
        &push_range,
        "ancestor7",
        "child7",
        "wip: half the fix",
        "op-squash-1",
    ));
    // Loop re-entry: nothing conflicted left, so the collapse finishes.
    cmds.push(conflicts_in_revset_cmd(&push_range, &[]));
    // Result gate: wider range clean, and the tree is byte-identical.
    cmds.push(conflicts_in_revset_cmd("main@github..@", &[]));
    cmds.push(tree_delta_cmd("headbefore", branch, ""));
    // Verified — restore the message `--use-destination-message` dropped.
    cmds.push(description_cmd(branch, "the real change\n"));
    cmds.push(describe_cmd(
        branch,
        &merged_description("the real change", &["wip: half the fix"]),
    ));
    cmds.push(op_id_cmd("op-describe\n"));
    cmds.extend(push_and_verify_cmds(branch));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("replay-only rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "clean");
    assert_eq!(result.payload["pushed"], true);
    assert_eq!(
        result.payload["linearized_commits"], 1,
        "the collapse must be reported on the payload, never silent"
    );
    assert_eq!(
        result.payload["linearized_descriptions"][0], "wip: half the fix",
        "the collapsed commit's message must be preserved, not discarded"
    );
    assert!(result.message.starts_with("REBASED_CLEAN"));
    assert!(
        result.message.contains("collapsed forward"),
        "the human message must disclose that history shape changed: {}",
        result.message
    );
}

/// The `--no-push` path must disclose the collapse just as loudly as the
/// pushing one — the branch is still rewritten, the caller just pushes it
/// itself.
#[test]
fn rebase_no_push_reports_the_collapse_on_the_message_and_payload() {
    let branch = "boss/exec_replay_nopush";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "n1"),
        positioned_cmd(branch, "n1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["npanc"]));
    cmds.extend(preflight_cmds(branch, "nphead", &[("npanc", "npchild")]));
    cmds.extend(collapse_round_cmds(
        &push_range,
        "npanc",
        "npchild",
        "np: earlier step",
        "op-np-1",
    ));
    cmds.push(conflicts_in_revset_cmd(&push_range, &[]));
    cmds.push(conflicts_in_revset_cmd("main@github..@", &[]));
    cmds.push(tree_delta_cmd("nphead", branch, ""));
    cmds.push(description_cmd(branch, "np: the real change\n"));
    cmds.push(describe_cmd(
        branch,
        &merged_description("np: the real change", &["np: earlier step"]),
    ));
    cmds.push(op_id_cmd("op-np-describe\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, true)).expect("no-push replay-only rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "clean");
    assert_eq!(result.payload["pushed"], false);
    assert_eq!(result.payload["linearized_commits"], 1);
    assert_eq!(result.payload["linearized_descriptions"][0], "np: earlier step");
    assert!(
        result.message.contains("collapsed forward"),
        "--no-push must disclose the collapse too: {}",
        result.message
    );
}

/// A conflicted head is a real content conflict. It must hand off even
/// though ancestors are conflicted too — the collapse must never be a way
/// to make a genuine conflict "succeed".
#[test]
fn rebase_declines_collapse_when_the_pushed_head_is_itself_conflicted() {
    let branch = "boss/exec_realconflict";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "h1"),
        positioned_cmd(branch, "h1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["headx", "ancestorx"]));
    cmds.push(head_conflict_probe_cmd(branch, "headx\n"));
    cmds.push(resolve_list_for_commit_cmd("headx", "src/a.rs\n"));
    cmds.push(resolve_list_for_commit_cmd("ancestorx", "src/b.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("real conflict rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["pushed"], false);
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(
        result.payload["linearize_decline"], "conflicted_head",
        "the guard that refused must be recorded, not flattened into a generic decline"
    );
    assert!(result.message.starts_with("REBASED_WITH_CONFLICTS"));
}

/// A bookmark on the conflicted ancestor makes it a stacked-PR boundary
/// another PR's head points at. Rewriting it would move that PR silently, so
/// the collapse declines and hands off — and because the guards are
/// pre-flighted, it declines before any squash lands, so nothing is undone.
#[test]
fn rebase_declines_collapse_when_a_conflicted_ancestor_carries_a_bookmark() {
    let branch = "boss/exec_stacked";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "s1"),
        positioned_cmd(branch, "s1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["lowerpr"]));
    cmds.push(head_conflict_probe_cmd(branch, ""));
    cmds.push(head_commit_id_cmd(branch, "stackhead\n"));
    cmds.push(conflicts_in_revset_cmd(&push_range, &["lowerpr"]));
    // Pre-flight catches the bookmark with the workspace still pristine.
    cmds.push(bookmarks_at_cmd("lowerpr", "boss/exec_lower\n"));
    cmds.push(resolve_list_for_commit_cmd("lowerpr", "src/stack.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("stacked rebase");
    // No `jj op undo` in the script: assert_exhausted proves none was needed.
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(result.payload["linearize_decline"], "bookmarked_ancestor");
    assert!(result.message.starts_with("REBASED_WITH_CONFLICTS"));
}

/// The dangerous sequence: a guard declines *after* a squash has already
/// landed (here a commit acquires both the conflict flag and a bookmark only
/// once content is squashed into it, so the pre-flight could not have seen
/// it). The rollback must undo exactly the operations this attempt issued —
/// `jj op undo <id>`, never `jj op restore`, which would reset the shared
/// repo every sibling cube workspace points at.
#[test]
fn rebase_undoes_only_its_own_operations_when_a_guard_declines_after_a_squash() {
    let branch = "boss/exec_midstack";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "m1"),
        positioned_cmd(branch, "m1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["low"]));
    cmds.extend(preflight_cmds(branch, "midhead", &[("low", "mid")]));
    cmds.extend(collapse_round_cmds(
        &push_range,
        "low",
        "mid",
        "low: wip",
        "op-squash-a",
    ));
    // Round two: a commit the pre-flight never saw is now conflicted, and it
    // carries a bookmark — decline with one squash on the ground.
    cmds.push(conflicts_in_revset_cmd(&push_range, &["mid2"]));
    cmds.push(bookmarks_at_cmd("mid2", "boss/exec_lower\n"));
    // Exactly one undo, for exactly the one operation this attempt issued.
    cmds.push(op_undo_cmd("op-squash-a"));
    cmds.push(resolve_list_for_commit_cmd("low", "src/mid.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("mid-loop decline");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(result.payload["linearize_decline"], "bookmarked_ancestor");
}

/// The iteration bound exists for a jj whose squash leaves the conflict flag
/// set: the loop must terminate, decline, and undo every squash it issued —
/// newest first — rather than spin.
#[test]
fn rebase_declines_and_undoes_every_squash_when_the_round_limit_is_exhausted() {
    let branch = "boss/exec_rounds";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "q1"),
        positioned_cmd(branch, "q1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["stuck"]));
    cmds.extend(preflight_cmds(branch, "roundhead", &[("stuck", "kid")]));
    // One conflicted commit sizes the bound at 2*1+1 rounds, so two squashes
    // land before the third round bails.
    cmds.extend(collapse_round_cmds(&push_range, "stuck", "kid", "stuck: wip", "op-r1"));
    cmds.extend(collapse_round_cmds(&push_range, "stuck", "kid", "stuck: wip", "op-r2"));
    cmds.push(conflicts_in_revset_cmd(&push_range, &["stuck"]));
    // Undo in reverse order: the newest operation first.
    cmds.push(op_undo_cmd("op-r2"));
    cmds.push(op_undo_cmd("op-r1"));
    cmds.push(resolve_list_for_commit_cmd("stuck", "src/stuck.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("round-limited rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(result.payload["linearize_decline"], "round_limit");
}

/// Result gate 1: the collapse loop drains `{main}..{branch}`, but the wider
/// `{main}..@` range that `jj git push` would actually reject still carries a
/// conflict. That is not a verified resolution — decline and undo.
#[test]
fn rebase_declines_collapse_when_the_wider_pushed_range_is_still_conflicted() {
    let branch = "boss/exec_wider";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "w1"),
        positioned_cmd(branch, "w1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["above"]));
    cmds.extend(preflight_cmds(branch, "widerhead", &[("anc", "ancchild")]));
    cmds.extend(collapse_round_cmds(&push_range, "anc", "ancchild", "anc: wip", "op-w1"));
    cmds.push(conflicts_in_revset_cmd(&push_range, &[]));
    // The branch range is clean but `@` above it is not.
    cmds.push(conflicts_in_revset_cmd("main@github..@", &["above"]));
    cmds.push(op_undo_cmd("op-w1"));
    cmds.push(resolve_list_for_commit_cmd("above", "src/above.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("wider-range rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(result.payload["linearize_decline"], "range_still_conflicted");
}

/// The result gate is the point of the whole thing: if the collapse would
/// change the pushed tree by so much as one path, the resolution is not
/// verifiable and must be discarded rather than pushed.
#[test]
fn rebase_declines_collapse_when_the_tree_would_change() {
    let branch = "boss/exec_treedrift";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "t1"),
        positioned_cmd(branch, "t1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["drifty"]));
    cmds.extend(preflight_cmds(branch, "driftbefore", &[("drifty", "driftchild")]));
    cmds.extend(collapse_round_cmds(
        &push_range,
        "drifty",
        "driftchild",
        "drift: wip",
        "op-drift-1",
    ));
    cmds.push(conflicts_in_revset_cmd(&push_range, &[]));
    cmds.push(conflicts_in_revset_cmd("main@github..@", &[]));
    // The tree moved — refuse to push it.
    cmds.push(tree_delta_cmd("driftbefore", branch, "M src/drift.rs\n"));
    // One squash had landed, so the decline owes an undo of exactly that
    // operation before the conflicts are reported.
    cmds.push(op_undo_cmd("op-drift-1"));
    cmds.push(resolve_list_for_commit_cmd("drifty", "src/drift.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("tree drift rebase");
    runner.assert_exhausted();
    assert_eq!(
        result.payload["status"], "conflicts",
        "a collapse that changes the tree must never be reported as a clean rebase"
    );
    assert_eq!(result.payload["pushed"], false);
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(result.payload["linearize_decline"], "tree_drift");
}

/// A conflicted ancestor with more than one child has no single place for
/// its content to go — decline rather than guess. Pre-flighted, so again no
/// rollback is owed.
#[test]
fn rebase_declines_collapse_when_a_conflicted_ancestor_has_multiple_children() {
    let branch = "boss/exec_fork";
    let push_range = format!("main@github..{branch}");
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "k1"),
        positioned_cmd(branch, "k1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["forky"]));
    cmds.push(head_conflict_probe_cmd(branch, ""));
    cmds.push(head_commit_id_cmd(branch, "forkhead\n"));
    cmds.push(conflicts_in_revset_cmd(&push_range, &["forky"]));
    cmds.push(bookmarks_at_cmd("forky", ""));
    cmds.push(children_of_cmd("forky", &["kid1", "kid2"]));
    cmds.push(resolve_list_for_commit_cmd("forky", "src/fork.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("fork rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearized_commits"], 0);
    assert_eq!(result.payload["linearize_decline"], "multiple_children");
}

/// The head snapshot coming back empty means there is no tree to verify
/// against, so the collapse must decline rather than proceed unverified.
#[test]
fn rebase_declines_collapse_when_the_head_does_not_resolve() {
    let branch = "boss/exec_nohead";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "z1"),
        positioned_cmd(branch, "z1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["ghost"]));
    cmds.push(head_conflict_probe_cmd(branch, ""));
    cmds.push(head_commit_id_cmd(branch, ""));
    cmds.push(resolve_list_for_commit_cmd("ghost", "src/ghost.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("no-head rebase");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearize_decline"], "no_head");
}

/// The collapse is an optimisation over an already-usable conflict report,
/// so an anomalous jj failure inside it must never destroy that report. It
/// falls back to the ordinary hand-off — with the conflicted-file list
/// intact, which is what lets the ladder still reach its cheaper rungs —
/// rather than failing the whole rebase.
#[test]
fn rebase_falls_back_to_the_conflict_handoff_when_a_collapse_probe_fails() {
    let branch = "boss/exec_probefail";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "p1"),
        positioned_cmd(branch, "p1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &["pf"]));
    cmds.push(failing_cmd(
        rebase_cwd(),
        "jj",
        &["log", "-r", branch, "--no-graph", "-T", CONFLICT_COMMITS_TMPL],
        "Error: jj exploded",
    ));
    cmds.push(resolve_list_for_commit_cmd("pf", "src/pf.rs\n"));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("probe failure must not abort");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "conflicts");
    assert_eq!(result.payload["linearize_decline"], "probe_error");
    assert_eq!(
        result.payload["conflicted_files"][0], "src/pf.rs",
        "the residual file list must survive a probe failure — the ladder routes on it"
    );
}

/// A clean rebase must not pay for any of this: no head probe, no collapse
/// machinery, straight to the push tail.
#[test]
fn rebase_clean_path_runs_no_linearization_probes() {
    let branch = "boss/exec_untouched";
    let mut cmds = vec![
        fetch_cmd(),
        remote_exists_cmd(branch, "u1"),
        positioned_cmd(branch, "u1"),
    ];
    cmds.extend(set_track_rebase_check_cmds(branch, &[]));
    cmds.extend(push_and_verify_cmds(branch));

    let runner = FakeRunner::new(cmds);
    let result = run_rebase(&runner, &rebase_opts(Some(branch), None, false)).expect("clean rebase");
    // assert_exhausted is the real assertion: any extra probe would leave
    // the script unconsumed or fail on an unexpected command.
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "clean");
    assert_eq!(result.payload["linearized_commits"], 0);
}

// ───────────────────────── workspace push ─────────────────────────
//
// `workspace_push` is the testable unit for `cube workspace push`. Like
// `pr_push`, it reads `std::env::current_dir()` directly rather than
// taking an explicit `cwd` param, so these tests capture the ambient
// test-process cwd (matching the `pr_push` test convention) instead of
// a synthetic path.
