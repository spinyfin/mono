use super::checkleft_tests::CheckleftEnvGuard;
use super::pr_push_tests::remote_list_github;
use super::rebase_tests::{CONFLICT_TMPL, failing_cmd};
use super::support::ENV_MUTEX;
use super::support::{ExpectedCommand, FakeRunner};

use crate::app::errors::Result;
use crate::app::workspace_ops::workspace_push;

/// `workspace_push` (the testable core of `cube workspace push`, including
/// rung-0 conflict-ladder pushes) runs the real checkleft push gate against
/// the real environment, not through `FakeRunner`. Disable it while holding
/// `ENV_MUTEX` so concurrently-running checkleft-resolution tests that
/// mutate global `PATH` / `CUBE_CHECKLEFT_BIN` can never make this test flaky.
fn run_push(runner: &FakeRunner, bookmark: Option<&str>, pr: Option<u64>) -> Result<crate::app::errors::RunResult> {
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    workspace_push(None, runner, bookmark.map(str::to_string), pr)
}

#[test]
fn workspace_push_happy_path_advances_and_pushes() {
    let cwd = std::env::current_dir().expect("cwd");
    let branch = "boss/exec_abc";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", CONFLICT_TMPL],
            "CLEAN",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "description"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["describe", "-r", "@", "-m", "Resolve merge conflict on boss/exec_abc"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["bookmark", "set", branch, "-r", "@", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", branch, "--remote", "github"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
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

    let result = run_push(&runner, Some(branch), None).expect("push happy path");
    runner.assert_exhausted();
    assert_eq!(result.payload["status"], "pushed");
    assert_eq!(result.payload["branch"], branch);
    assert_eq!(result.payload["pushed"], true);
    assert!(result.message.starts_with("PUSHED"));
}

#[test]
fn workspace_push_pr_arg_resolves_head_branch_from_github() {
    let cwd = std::env::current_dir().expect("cwd");
    let branch = "boss/exec_pr9";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["pr", "view", "9", "-R", "spinyfin/mono", "--json", "headRefName"],
            r#"{"headRefName":"boss/exec_pr9"}"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", CONFLICT_TMPL],
            "CLEAN",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "description"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "describe",
                "-r",
                "@",
                "-m",
                "Resolve merge conflict on PR #9 (boss/exec_pr9)",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["bookmark", "set", branch, "-r", "@", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", branch, "--remote", "github"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
            "cafe\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_pr9",
                "--jq",
                ".commit.sha",
            ],
            "cafe\n",
        ),
    ]);

    let result = run_push(&runner, None, Some(9)).expect("push via --pr");
    runner.assert_exhausted();
    assert_eq!(result.payload["branch"], branch);
}

// Regression coverage: `jj git push` refuses a commit with no
// description. `@` reaches `cube workspace push` undescribed when the
// conflict-ladder's rung-0 deterministic resolvers edited files directly
// without ever calling `jj describe` — see `attempt_rung0` in
// `conflict_ladder.rs`. This test pins that a blank description is
// detected and stamped with a deterministic message before the bookmark
// is advanced and pushed.
#[test]
fn workspace_push_describes_an_undescribed_working_copy_before_pushing() {
    let cwd = std::env::current_dir().expect("cwd");
    let branch = "boss/exec_undescribed";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", CONFLICT_TMPL],
            "CLEAN",
        ),
        // Blank description (only whitespace) — must still trigger `jj describe`.
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "description"],
            "  \n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "describe",
                "-r",
                "@",
                "-m",
                "Resolve merge conflict on boss/exec_undescribed",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["bookmark", "set", branch, "-r", "@", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", branch, "--remote", "github"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
            "beef\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_undescribed",
                "--jq",
                ".commit.sha",
            ],
            "beef\n",
        ),
    ]);

    let result = run_push(&runner, Some(branch), None).expect("push describes then pushes");
    runner.assert_exhausted();
    assert_eq!(result.payload["pushed"], true);
}

// Sibling of the test above: an already-described `@` (e.g. a human or
// an earlier step already set one) must be left untouched — no
// `jj describe` call at all.
#[test]
fn workspace_push_leaves_an_existing_description_untouched() {
    let cwd = std::env::current_dir().expect("cwd");
    let branch = "boss/exec_described";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", CONFLICT_TMPL],
            "CLEAN",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "description"],
            "an existing commit message\n",
        ),
        // No `describe` call expected here.
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["bookmark", "set", branch, "-r", "@", "--allow-backwards"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", branch, "--remote", "github"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", branch, "--no-graph", "-T", "commit_id"],
            "d00d\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "api",
                "repos/spinyfin/mono/branches/boss/exec_described",
                "--jq",
                ".commit.sha",
            ],
            "d00d\n",
        ),
    ]);

    let result = run_push(&runner, Some(branch), None).expect("push with existing description");
    runner.assert_exhausted();
    assert_eq!(result.payload["pushed"], true);
}

#[test]
fn workspace_push_refuses_unresolved_conflicts_without_pushing() {
    let cwd = std::env::current_dir().expect("cwd");
    let branch = "boss/exec_conflicted";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", CONFLICT_TMPL],
            "CONFLICT",
        ),
        // No bookmark-set / push commands expected — the conflict check
        // must short-circuit before either.
    ]);

    let err = run_push(&runner, Some(branch), None).expect_err("must refuse unresolved conflicts");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(msg.contains("unresolved conflicts"), "{msg}");
    assert!(msg.contains("jj resolve --list"), "{msg}");
}

#[test]
fn workspace_push_failure_surfaces_clear_error() {
    let cwd = std::env::current_dir().expect("cwd");
    let branch = "boss/exec_pf2";
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(cwd.clone(), "jj", &["git", "remote", "list"], remote_list_github()),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", CONFLICT_TMPL],
            "CLEAN",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "description"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["describe", "-r", "@", "-m", "Resolve merge conflict on boss/exec_pf2"],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["bookmark", "set", branch, "-r", "@", "--allow-backwards"],
            "",
        ),
        failing_cmd(
            cwd.clone(),
            "jj",
            &["git", "push", "-b", branch, "--remote", "github"],
            "Error: remote bookmark changed",
        ),
    ]);

    let err = run_push(&runner, Some(branch), None).expect_err("push failure must surface");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(msg.contains("failed to push"), "{msg}");
    assert!(msg.contains(branch), "{msg}");
}

// ───────────────────────── workspace goto ─────────────────────────
//
// `workspace_goto` is the testable core of `cube workspace goto`:
// fetch → resolve branch → probe existence → set bookmarks →
// idempotency check → `jj new` (or skip when already positioned).
// Tests drive it via `workspace_goto(None, &runner, Some(GOTO_CWD.into()), ...)`.
