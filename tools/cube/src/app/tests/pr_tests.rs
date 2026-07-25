use super::checkleft_tests::CheckleftEnvGuard;
use super::support::ENV_MUTEX;
use super::support::{ExpectedCommand, FakeRunner};
use clap::Parser;
use tempfile::TempDir;

use crate::cli::Cli;

use crate::app::dispatch::run_with_dependencies;

#[test]
fn ensure_pr_uses_body_file_flag_not_body_flag() {
    // Regression: when --body-file is given, gh pr create must receive
    // --body-file <path>, NOT --body <content>. Passing the body inline
    // via --body "..." lets the shell evaluate backticks and $(...) before
    // cube ever sees the argument, corrupting PR bodies that contain
    // inline code.
    let body_content = "Use `rustc --help` or `$(cargo --version)` or ${CARGO_HOME}.\n\nMore `code`.";
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(tmp.path(), body_content).expect("write body");
    let body_path = tmp.path().display().to_string();

    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        // Push verification: local commit vs GitHub branch head sha.
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            "[]",
        ),
        // The critical assertion: gh receives --body-file <path>, not --body <content>.
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "create",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--base",
                "main",
                "--title",
                "Test PR",
                "--body-file",
                &body_path,
            ],
            "https://github.com/spinyfin/mono/pull/99",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/99", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "ensure",
        "--branch",
        "my-feature",
        "--title",
        "Test PR",
        "--body-file",
        &body_path,
    ]);
    let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr with --body-file");
    runner.assert_exhausted();

    assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/99");
    // Body file must not be modified — backticks and $(...) survive verbatim.
    let body_on_disk = std::fs::read_to_string(tmp.path()).expect("read body");
    assert_eq!(body_on_disk, body_content);
}

// --- ensure_pr JSON output shape tests ---

#[test]
fn ensure_pr_created_json_has_action_url_number() {
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            "[]",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "create",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--base",
                "main",
                "--title",
                "New PR",
            ],
            "https://github.com/spinyfin/mono/pull/42",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/42", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "ensure", "--branch", "my-feature", "--title", "New PR"]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr created");
    runner.assert_exhausted();

    assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/42");
    assert_eq!(result.payload["action"], "created");
    assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/42");
    assert_eq!(result.payload["number"], 42);
    assert_eq!(result.payload["pr_bookmark"], "pr/42");
}

#[test]
fn ensure_pr_exists_json_has_action_url_number() {
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/7"}]"#,
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/7", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "ensure",
        "--branch",
        "my-feature",
        "--title",
        "Existing PR",
    ]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr exists");
    runner.assert_exhausted();

    assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/7");
    assert_eq!(result.payload["action"], "exists");
    assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/7");
    assert_eq!(result.payload["number"], 7);
    assert_eq!(result.payload["pr_bookmark"], "pr/7");
}

#[test]
fn ensure_pr_pushes_to_github_remote_not_local_mirror() {
    // Regression for the "false push" trap: a cube workspace has a local
    // mirror named `origin` and the real GitHub upstream named `github`.
    // `cube pr ensure` must push to `github` (resolved by URL, not by the
    // conventional `origin` name) and verify the push against GitHub.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\t/Users/bduff/dev/agents/repos/mono\n\
                 github\tgit@github.com:spinyfin/mono.git\n",
        ),
        // Push targets `github`, the real upstream — NOT the local mirror.
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "github",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "deadbeef\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "deadbeef\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            "[]",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "create",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--base",
                "main",
                "--title",
                "New PR",
            ],
            "https://github.com/spinyfin/mono/pull/77",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/77", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "ensure", "--branch", "my-feature", "--title", "New PR"]);
    let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr created");
    runner.assert_exhausted();
    assert_eq!(result.payload["url"], "https://github.com/spinyfin/mono/pull/77");
}

#[test]
fn ensure_pr_fails_loudly_when_push_did_not_reach_github() {
    // The push "succeeded" locally but GitHub's branch head sha does not
    // match the local commit — the classic local-mirror false positive.
    // ensure_pr must error rather than report a stale PR as updated.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "4ce6198\n",
        ),
        // GitHub still has the OLD sha — push never reached it.
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "2f8dd09\n",
        ),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "ensure", "--branch", "my-feature", "--title", "New PR"]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let err =
        run_with_dependencies(cli, None, &runner).expect_err("ensure_pr should fail when push did not reach GitHub");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(
        msg.contains("push verification failed") && msg.contains("4ce6198") && msg.contains("2f8dd09"),
        "error should name the mismatch loudly: {msg}"
    );
}

#[test]
fn ensure_pr_guard_rejects_pr_bookmark_branch_arg() {
    // Passing --branch pr/42 to `cube pr ensure` must be refused before any
    // push is attempted — the `pr/<n>` namespace is reserved as local-only.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        // No further commands: the guard fires before jj git push.
    ]);

    let cli = Cli::parse_from(["cube", "pr", "ensure", "--branch", "pr/42"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("ensure_pr should refuse a pr/* branch");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(
        msg.contains("pr/42") && msg.contains("reserved"),
        "error should mention the bookmark and reserved: {msg}"
    );
}

#[test]
fn ensure_pr_sets_pr_bookmark_on_create() {
    // After a new PR is created, `jj bookmark set pr/<n> -r <branch>` must
    // be called so the workspace has a local pointer from number to head.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            "[]",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "create",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--base",
                "main",
                "--title",
                "My Feature",
            ],
            "https://github.com/spinyfin/mono/pull/55",
        ),
        // Must call `jj bookmark set pr/55 -r my-feature` after creation.
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/55", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "ensure",
        "--branch",
        "my-feature",
        "--title",
        "My Feature",
    ]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr create+bookmark");
    runner.assert_exhausted();

    assert_eq!(result.payload["pr_bookmark"], "pr/55");
}

#[test]
fn ensure_pr_sets_pr_bookmark_on_existing_pr() {
    // When a PR already exists (backfill path), `jj bookmark set pr/<n> -r
    // <branch>` must still be called so the local bookmark is up to date.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/33"}]"#,
        ),
        // Bookmark set must happen even on the reuse/backfill path.
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/33", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "ensure",
        "--branch",
        "my-feature",
        "--title",
        "My Feature",
    ]);
    let result = run_with_dependencies(cli, None, &runner).expect("ensure_pr exists+bookmark");
    runner.assert_exhausted();

    assert_eq!(result.payload["action"], "exists");
    assert_eq!(result.payload["pr_bookmark"], "pr/33");
}

#[test]
fn ensure_pr_errors_on_multiple_open_prs() {
    // If `gh pr list` returns more than one open PR for the branch, cube
    // must error rather than silently picking one. This is an unexpected
    // state that requires human intervention.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/10"},{"url":"https://github.com/spinyfin/mono/pull/11"}]"#,
        ),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "ensure",
        "--branch",
        "my-feature",
        "--title",
        "My Feature",
    ]);
    let _lock = ENV_MUTEX.lock().unwrap();
    let _env = CheckleftEnvGuard::with_gate_disabled();
    let err = run_with_dependencies(cli, None, &runner).expect_err("ensure_pr should fail on >1 open PRs");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(
        msg.contains("2") && msg.contains("my-feature"),
        "error should mention count and branch: {msg}"
    );
}

// --- pr_create / pr_update split tests ---

#[test]
fn pr_create_opens_pr_when_none_exists() {
    // `cube pr create` checks for an existing PR FIRST (fail-fast, no
    // side effects), then pushes and opens a new PR.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        // Existence check happens BEFORE the push.
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            "[]",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "create",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--base",
                "main",
                "--title",
                "New PR",
            ],
            "https://github.com/spinyfin/mono/pull/42",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/42", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "create", "--branch", "my-feature", "--title", "New PR"]);
    let result = run_with_dependencies(cli, None, &runner).expect("pr create");
    runner.assert_exhausted();

    assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/42");
    assert_eq!(result.payload["action"], "created");
    assert_eq!(result.payload["number"], 42);
    assert_eq!(result.payload["pr_bookmark"], "pr/42");
}

#[test]
fn pr_create_is_idempotent_when_pr_already_exists() {
    // Retry-safety contract: if a prior `cube pr create` invocation's
    // push actually landed after the CALLER gave up on it (e.g. a tool's
    // own output-silence timeout killed the original call), a bare retry
    // of `cube pr create` must succeed with the existing PR's URL rather
    // than erroring — and it must NOT push again.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/7"}]"#,
        ),
        // No push, no `gh pr create` — only the local pr/<n> bookmark is set.
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/7", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "create", "--branch", "my-feature", "--title", "Dup"]);
    let result = run_with_dependencies(cli, None, &runner).expect("pr create must succeed when PR already exists");
    runner.assert_exhausted();

    assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/7");
    assert_eq!(result.payload["action"], "already_exists");
    assert_eq!(result.payload["number"], 7);
    assert_eq!(result.payload["pr_bookmark"], "pr/7");
}

#[test]
fn pr_update_pushes_when_pr_exists() {
    // `cube pr update` pushes new commits to the existing PR's branch and
    // never calls `gh pr create`.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/7"}]"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/7", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "update", "--branch", "my-feature"]);
    let result = run_with_dependencies(cli, None, &runner).expect("pr update");
    runner.assert_exhausted();

    assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/7");
    assert_eq!(result.payload["action"], "updated");
    assert_eq!(result.payload["number"], 7);
    assert_eq!(result.payload["pr_bookmark"], "pr/7");
}

#[test]
fn pr_update_errors_when_no_pr_exists() {
    // `cube pr update` must refuse — and must NOT push — when no open PR
    // exists, pointing the caller at `cube pr create`.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            "[]",
        ),
        // No push — the missing-PR check short-circuits.
    ]);

    let cli = Cli::parse_from(["cube", "pr", "update", "--branch", "my-feature"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("pr update must error when no PR");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(
        msg.contains("no open PR") && msg.contains("cube pr create") && msg.contains("my-feature"),
        "error should say no PR and point at `cube pr create`: {msg}"
    );
}

#[test]
fn pr_update_refuses_when_working_copy_is_stale() {
    // T2764 postmortem: a worker rebased the bookmark onto main but never
    // `jj edit`ed into it, so `@` stayed on the old tree while the
    // bookmark advanced. `cube pr update` must refuse before pushing —
    // and before even checking whether a PR exists — rather than ship a
    // commit that was never actually gated.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "5bad2f0\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            "main000\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            "mainparent\n",
        ),
        // No gh pr list, no push — the working-copy check short-circuits first.
    ]);

    let cli = Cli::parse_from(["cube", "pr", "update", "--branch", "my-feature"]);
    let err = run_with_dependencies(cli, None, &runner).expect_err("pr update must refuse a stale working copy");
    runner.assert_exhausted();
    let msg = err.to_string();
    assert!(
        msg.contains("my-feature")
            && msg.contains("5bad2f0")
            && msg.contains("main000")
            && msg.contains("jj edit my-feature")
            && msg.contains("--allow-detached-push"),
        "error should name both commits and the remedy: {msg}"
    );
}

#[test]
fn pr_update_proceeds_when_working_copy_is_empty_child_of_branch_head() {
    // The common shape after `jj new <bookmark>`: `@` is a fresh empty
    // commit whose parent (`@-`) is the bookmark head. This must be
    // treated as equivalent to being checked out on the branch itself.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "commit_id"],
            "def456\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@-", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "@", "--no-graph", "-T", "if(empty, \"true\", \"false\")"],
            "true\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/7"}]"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/7", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from(["cube", "pr", "update", "--branch", "my-feature"]);
    let result = run_with_dependencies(cli, None, &runner)
        .expect("pr update must proceed when @ is an empty child of the branch head");
    runner.assert_exhausted();

    assert_eq!(result.message, "https://github.com/spinyfin/mono/pull/7");
    assert_eq!(result.payload["action"], "updated");
}

#[test]
fn pr_update_allow_detached_push_skips_working_copy_check() {
    // The escape hatch: with --allow-detached-push, no working-copy
    // check commands run at all before the push proceeds.
    let cwd = std::env::current_dir().expect("cwd");
    let runner = FakeRunner::new(vec![
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["git", "remote", "list"],
            "origin\tgit@github.com:spinyfin/mono.git\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &[
                "pr",
                "list",
                "-R",
                "spinyfin/mono",
                "--head",
                "my-feature",
                "--state",
                "open",
                "--json",
                "url",
            ],
            r#"[{"url":"https://github.com/spinyfin/mono/pull/7"}]"#,
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &[
                "git",
                "push",
                "-b",
                "my-feature",
                "--remote",
                "origin",
                "--allow-new",
                "--ignore-working-copy",
            ],
            "",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "jj",
            &["log", "-r", "my-feature", "--no-graph", "-T", "commit_id"],
            "abc123\n",
        ),
        ExpectedCommand::ok(
            cwd.clone(),
            "gh",
            &["api", "repos/spinyfin/mono/branches/my-feature", "--jq", ".commit.sha"],
            "abc123\n",
        ),
        ExpectedCommand::ok(cwd.clone(), "jj", &["bookmark", "set", "pr/7", "-r", "my-feature"], ""),
    ]);

    let cli = Cli::parse_from([
        "cube",
        "pr",
        "update",
        "--branch",
        "my-feature",
        "--allow-detached-push",
    ]);
    let result = run_with_dependencies(cli, None, &runner).expect("pr update with --allow-detached-push");
    runner.assert_exhausted();

    assert_eq!(result.payload["action"], "updated");
}

// --- pr_push tests ---

#[test]
fn pr_number_from_url_parses_standard_url() {
    assert_eq!(
        crate::app::pr::pr_number_from_url("https://github.com/owner/repo/pull/123"),
        Some(123)
    );
}

#[test]
fn pr_number_from_url_parses_url_with_trailing_slash() {
    assert_eq!(
        crate::app::pr::pr_number_from_url("https://github.com/owner/repo/pull/456/"),
        Some(456)
    );
}

#[test]
fn pr_number_from_url_returns_none_for_non_numeric_suffix() {
    assert_eq!(
        crate::app::pr::pr_number_from_url("https://github.com/owner/repo/pull/"),
        None
    );
}

fn pr_create_args(body: Option<&str>, body_file: Option<&str>) -> crate::cli::PrCreateArgs {
    crate::cli::PrCreateArgs {
        branch: None,
        title: None,
        body: body.map(str::to_string),
        body_file: body_file.map(str::to_string),
        draft: false,
    }
}

#[test]
fn resolve_pr_body_uses_inline_body_flag() {
    let args = pr_create_args(Some("PR body text"), None);
    let resolved = crate::app::pr::resolve_pr_body(&args).unwrap();
    assert_eq!(resolved.text.as_deref(), Some("PR body text"));
    assert_eq!(resolved.file_path, None);
}

#[test]
fn resolve_pr_body_reads_body_file_from_regular_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("body.md");
    std::fs::write(&path, "body from file\n").unwrap();

    let path_str = path.to_str().unwrap();
    let args = pr_create_args(None, Some(path_str));
    let resolved = crate::app::pr::resolve_pr_body(&args).unwrap();
    assert_eq!(resolved.text.as_deref(), Some("body from file\n"));
    assert_eq!(resolved.file_path.as_deref(), Some(path_str));
}

#[test]
fn resolve_pr_body_is_none_when_neither_flag_supplied() {
    let args = pr_create_args(None, None);
    let resolved = crate::app::pr::resolve_pr_body(&args).unwrap();
    assert_eq!(resolved.text, None);
    assert_eq!(resolved.file_path, None);
}
